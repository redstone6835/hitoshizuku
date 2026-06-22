//! 块设备文件用户接口适配。
//!
//! `BLK*` ioctl 号、历史 512 字节扇区单位和用户指针写入都属于用户 ABI。
//! devtmpfs 只持有 typed `BlockDevice`，通过本模块把 ABI 请求翻译为
//! `BlockControlRequest` 和块设备 I/O hint。

use errno::Errno;
use vfs::file::IoctlCmd;

use crate::dev::bio::{BioOp, BlockRange};
use crate::dev::control::{BlockControlRequest, BlockControlResponse, BlockIoHints};

use super::ioctl::{
    read_bytes_from_user, write_i32_to_user, write_u32_to_user, write_u64_to_user,
    write_usize_to_user,
};

const BLKROGET: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 94, 0).raw();
const BLKGETSIZE: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 96, 0).raw();
const BLKFLSBUF: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 97, 0).raw();
const BLKSSZGET: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 104, 0).raw();
const BLKBSZGET: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, 0x12, 112, core::mem::size_of::<usize>()).raw();
const BLKDISCARD: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 119, 0).raw();
const BLKGETSIZE64: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, 0x12, 114, core::mem::size_of::<usize>()).raw();
const BLKIOMIN: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 120, 0).raw();
const BLKIOOPT: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 121, 0).raw();
const BLKALIGNOFF: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 122, 0).raw();
const BLKPBSZGET: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 123, 0).raw();
const BLKDISCARDZEROES: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 124, 0).raw();
const BLKROTATIONAL: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 126, 0).raw();
const BLKZEROOUT: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 127, 0).raw();
const BLKGETDISKSEQ: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, 0x12, 128, core::mem::size_of::<u64>()).raw();

const BLKGETSIZE_SECTOR_BYTES: u64 = 512;

/// 块设备 ioctl 适配需要的 typed control 上下文。
pub trait BlockDeviceIoctlContext {
    fn control(&self, req: BlockControlRequest) -> Result<BlockControlResponse, Errno>;
    fn io_hints(&self) -> Result<BlockIoHints, Errno>;
    fn submit_range(&self, op: BioOp, range: BlockRange) -> Result<(), Errno>;
}

/// 执行通用块设备 ioctl。
pub fn handle_block_ioctl<C: BlockDeviceIoctlContext>(
    ctx: &C,
    cmd: IoctlCmd,
    arg: usize,
) -> Result<usize, Errno> {
    match cmd.raw() {
        BLKROGET => {
            let readonly = match ctx.control(BlockControlRequest::GetReadOnly)? {
                BlockControlResponse::Bool(value) => i32::from(value),
                _ => return Err(Errno::EINVAL),
            };
            write_i32_to_user(arg, readonly)?;
            Ok(0)
        }
        BLKGETSIZE => {
            let bytes = capacity_bytes(ctx)?;
            let sectors =
                usize::try_from(bytes / BLKGETSIZE_SECTOR_BYTES).map_err(|_| Errno::EINVAL)?;
            write_usize_to_user(arg, sectors)?;
            Ok(0)
        }
        BLKGETSIZE64 => {
            write_u64_to_user(arg, capacity_bytes(ctx)?)?;
            Ok(0)
        }
        BLKSSZGET => {
            write_u32_to_user(arg, logical_block_size(ctx)?)?;
            Ok(0)
        }
        BLKBSZGET => {
            write_usize_to_user(arg, logical_block_size(ctx)? as usize)?;
            Ok(0)
        }
        BLKPBSZGET => {
            let block_size = match ctx.control(BlockControlRequest::GetPhysicalBlockSize)? {
                BlockControlResponse::U32(value) => value,
                _ => return Err(Errno::EINVAL),
            };
            write_u32_to_user(arg, block_size)?;
            Ok(0)
        }
        BLKIOMIN => {
            let hints = ctx.io_hints()?;
            write_u32_to_user(arg, hints.min_io_size)?;
            Ok(0)
        }
        BLKIOOPT => {
            let hints = ctx.io_hints()?;
            write_u32_to_user(arg, hints.optimal_io_size)?;
            Ok(0)
        }
        BLKALIGNOFF => {
            let hints = ctx.io_hints()?;
            write_i32_to_user(arg, hints.alignment_offset)?;
            Ok(0)
        }
        BLKDISCARDZEROES => {
            let hints = ctx.io_hints()?;
            write_i32_to_user(arg, i32::from(hints.discard_zeroes))?;
            Ok(0)
        }
        BLKDISCARD => {
            if let Some(range) = read_user_block_range(ctx, arg)? {
                ctx.submit_range(BioOp::Discard, range)?;
            }
            Ok(0)
        }
        BLKZEROOUT => {
            if let Some(range) = read_user_block_range(ctx, arg)? {
                ctx.submit_range(BioOp::WriteZeroes, range)?;
            }
            Ok(0)
        }
        BLKROTATIONAL => {
            let hints = ctx.io_hints()?;
            write_i32_to_user(arg, i32::from(hints.rotational))?;
            Ok(0)
        }
        BLKGETDISKSEQ => {
            let diskseq = match ctx.control(BlockControlRequest::GetDiskSeq)? {
                BlockControlResponse::U64(value) => value,
                _ => return Err(Errno::EINVAL),
            };
            write_u64_to_user(arg, diskseq)?;
            Ok(0)
        }
        BLKFLSBUF => {
            let _ = ctx.control(BlockControlRequest::Flush)?;
            Ok(0)
        }
        _ => Err(Errno::ENOTTY),
    }
}

fn capacity_bytes<C: BlockDeviceIoctlContext>(ctx: &C) -> Result<u64, Errno> {
    match ctx.control(BlockControlRequest::GetCapacityBytes)? {
        BlockControlResponse::U64(value) => Ok(value),
        _ => Err(Errno::EINVAL),
    }
}

fn logical_block_size<C: BlockDeviceIoctlContext>(ctx: &C) -> Result<u32, Errno> {
    match ctx.control(BlockControlRequest::GetLogicalBlockSize)? {
        BlockControlResponse::U32(value) => Ok(value),
        _ => Err(Errno::EINVAL),
    }
}

fn read_user_block_range<C: BlockDeviceIoctlContext>(
    ctx: &C,
    arg: usize,
) -> Result<Option<BlockRange>, Errno> {
    let mut raw = [0u8; core::mem::size_of::<u64>() * 2];
    read_bytes_from_user(arg, &mut raw)?;
    let start = read_le_u64(&raw[..core::mem::size_of::<u64>()]);
    let len = read_le_u64(&raw[core::mem::size_of::<u64>()..]);
    if len == 0 {
        return Ok(None);
    }

    let block_size = u64::from(logical_block_size(ctx)?);
    if block_size == 0 || !start.is_multiple_of(block_size) || !len.is_multiple_of(block_size) {
        return Err(Errno::EINVAL);
    }
    let end = start.checked_add(len).ok_or(Errno::EINVAL)?;
    if end > capacity_bytes(ctx)? {
        return Err(Errno::EINVAL);
    }
    let blocks = u32::try_from(len / block_size).map_err(|_| Errno::EINVAL)?;
    Ok(Some(BlockRange {
        lba: start / block_size,
        blocks,
    }))
}

fn read_le_u64(bytes: &[u8]) -> u64 {
    let mut out = [0u8; core::mem::size_of::<u64>()];
    out.copy_from_slice(bytes);
    u64::from_le_bytes(out)
}
