//! 块设备分区表解析与分区块设备构造。
//!
//! 这里做 MBR/GPT 的最小解析:读整盘保护性 MBR(512B)与 GPT 头/条目数组,把每个
//! 分区包装成一个 remap 到父设备 + LBA 偏移的 [`BlockDevice`](crate::dev::block::BlockDevice)。
//! 分区设备在 VFS 投影层以 `/dev/<disk><part>` 形式暴露(见 device_files/projection)。
//!
//! 取舍(最小实现,无法完整复刻 Linux 语义):
//! - MBR 只在逻辑块大小 == 512 时解析,且不递归展开扩展分区(0x05/0x0f/0x85)。
//! - GPT 条目数组按设备逻辑块大小读取,`first_lba/last_lba` 直接按逻辑块解释。
//! - 分区驱动通过"gather 到连续缓冲 → 同步转发父设备"实现 remap,多一次 memcpy;
//!   异步 BIO 也退化为同步完成。这是可观测性能取舍,不改变块层 ABI。

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Write;
use core::num::NonZeroU32;

use vfs::sync::Spinlock;

use crate::dev::bio::{Bio, BioBuffer, BioError, BioIoError, BioOp, BlockRange, SubmitError};
use crate::dev::block::{BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockGeometry};
use crate::dev::control::{BlockControlRequest, BlockControlResponse, ControlError};

const MBR_BOOT_LEN: usize = 512;
const GPT_HEADER_LEN: usize = 512;
const GPT_MAGIC: &[u8; 8] = b"EFI PART";
const MBR_PARTITION_TABLE_OFFSET: usize = 446;
const MBR_ENTRY_LEN: usize = 16;
const MBR_PRIMARY_COUNT: usize = 4;
const GPT_MIN_ENTRY_SIZE: usize = 128;
const GPT_MAX_ENTRY_BYTES: usize = 1024 * 1024;

/// 分区表解析错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionError {
    Io,
    Invalid,
    OutOfMemory,
}

/// 一个已解析的分区块设备。
#[derive(Clone)]
pub struct PartitionDevice {
    dev: Arc<BlockDevice>,
    number: u32,
}

impl PartitionDevice {
    pub fn dev(&self) -> Arc<BlockDevice> {
        Arc::clone(&self.dev)
    }

    /// 分区序号(1-based,MBR 主分区槽位或 GPT 条目槽位)。
    pub fn number(&self) -> u32 {
        self.number
    }
}

/// 磁盘名 -> 分区节点名(`vd0` -> `vd0p1`,`sda` -> `sda1`)。
///
/// 与 Linux 命名规则一致:盘名以数字结尾时插入 `p` 再拼序号,否则直接拼序号。
pub fn partition_disk_name(disk_name: &str, number: u32) -> String {
    let mut name = String::new();
    // 命名分配失败时返回空串;调用方(VFS 投影层)会跳过该分区,避免 panic。
    if name.try_reserve(disk_name.len() + 16).is_err() {
        return String::new();
    }
    name.push_str(disk_name);
    if disk_name.bytes().last().is_some_and(|b| b.is_ascii_digit()) {
        name.push('p');
    }
    let _ = write!(&mut name, "{}", number);
    name
}

/// 分区表缓存(父设备名 -> 分区设备快照)。
///
/// 只在首次投影时扫描一次磁盘;父设备注销后再次查询会命中 `!is_active()` 分支并
/// 清除缓存项,避免热插拔泄漏旧分区对象。
static PARTITION_CACHE: Spinlock<BTreeMap<String, Vec<PartitionDevice>>> =
    Spinlock::new(BTreeMap::new());

/// 返回一个整盘块设备当前可用的分区设备(带缓存)。
pub fn partitions_of(parent: &Arc<BlockDevice>) -> Vec<PartitionDevice> {
    if parent.class() != BlockClass::Whole {
        return Vec::new();
    }
    if !parent.is_active() {
        let mut cache = PARTITION_CACHE.lock();
        cache.remove(parent.name());
        return Vec::new();
    }
    if let Some(parts) = PARTITION_CACHE.lock().get(parent.name()) {
        return parts.clone();
    }

    let parts = scan_partitions(parent);
    let mut key = String::new();
    if key.try_reserve(parent.name().len()).is_ok() {
        key.push_str(parent.name());
        PARTITION_CACHE.lock().insert(key, parts.clone());
    }
    parts
}

fn scan_partitions(parent: &Arc<BlockDevice>) -> Vec<PartitionDevice> {
    // Linux 优先识别 GPT(保护性 MBR 的 0xEE 只表示"存在 GPT")。
    if let Ok(parts) = scan_gpt(parent)
        && !parts.is_empty()
    {
        return parts;
    }
    scan_mbr(parent).unwrap_or_default()
}

fn scan_mbr(parent: &Arc<BlockDevice>) -> Result<Vec<PartitionDevice>, PartitionError> {
    if logical_block_size(parent)? != 512 {
        // MBR 的 LBA/扇区计数以 512B 扇区为单位;非 512B 逻辑块设备退回 GPT 路径。
        return Ok(Vec::new());
    }
    let mut boot = [0u8; MBR_BOOT_LEN];
    read_bytes_at(parent, 0, &mut boot)?;
    if boot[510] != 0x55 || boot[511] != 0xaa {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for index in 0..MBR_PRIMARY_COUNT {
        let entry = &boot[MBR_PARTITION_TABLE_OFFSET + index * MBR_ENTRY_LEN
            ..MBR_PARTITION_TABLE_OFFSET + (index + 1) * MBR_ENTRY_LEN];
        let type_code = entry[4];
        // 扩展分区(0x05/0x0f/0x85)包含逻辑分区链,最小实现不递归展开。
        if type_code == 0 || matches!(type_code, 0x05 | 0x0f | 0x85) {
            continue;
        }
        let start_lba = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as u64;
        let num_sectors = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as u64;
        if start_lba == 0 || num_sectors == 0 {
            continue;
        }
        out.push(make_partition(
            parent,
            index as u32 + 1,
            start_lba,
            num_sectors,
        )?);
    }
    Ok(out)
}

fn scan_gpt(parent: &Arc<BlockDevice>) -> Result<Vec<PartitionDevice>, PartitionError> {
    let block_size = logical_block_size(parent)? as u64;
    let mut header = [0u8; GPT_HEADER_LEN];
    read_bytes_at(parent, block_size, &mut header)?;
    if &header[0..8] != GPT_MAGIC {
        return Ok(Vec::new());
    }
    let entry_lba = u64::from_le_bytes(
        header[72..80]
            .try_into()
            .map_err(|_| PartitionError::Invalid)?,
    );
    let num_entries = u32::from_le_bytes(
        header[80..84]
            .try_into()
            .map_err(|_| PartitionError::Invalid)?,
    );
    let entry_size = u32::from_le_bytes(
        header[84..88]
            .try_into()
            .map_err(|_| PartitionError::Invalid)?,
    );
    if entry_size < GPT_MIN_ENTRY_SIZE as u32 || num_entries == 0 {
        return Ok(Vec::new());
    }
    let entry_size = entry_size as usize;
    let total = (num_entries as usize).saturating_mul(entry_size);
    if total == 0 || total > GPT_MAX_ENTRY_BYTES {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(total)
        .map_err(|_| PartitionError::OutOfMemory)?;
    entries.resize(total, 0);
    let entry_offset = entry_lba
        .checked_mul(block_size)
        .ok_or(PartitionError::Invalid)?;
    read_bytes_at(parent, entry_offset, &mut entries)?;

    let mut out = Vec::new();
    for index in 0..num_entries as usize {
        let entry = &entries[index * entry_size..index * entry_size + entry_size];
        let first_lba = u64::from_le_bytes(
            entry[32..40]
                .try_into()
                .map_err(|_| PartitionError::Invalid)?,
        );
        let last_lba = u64::from_le_bytes(
            entry[40..48]
                .try_into()
                .map_err(|_| PartitionError::Invalid)?,
        );
        if first_lba == 0 || last_lba < first_lba {
            continue;
        }
        let blocks = last_lba - first_lba + 1;
        out.push(make_partition(parent, index as u32 + 1, first_lba, blocks)?);
    }
    Ok(out)
}

fn make_partition(
    parent: &Arc<BlockDevice>,
    number: u32,
    start_lba: u64,
    blocks: u64,
) -> Result<PartitionDevice, PartitionError> {
    let logical = parent.geometry().logical_block_size().get();
    let physical = parent.geometry().physical_block_size().get();
    let geometry = BlockGeometry::new(
        NonZeroU32::new(logical).ok_or(PartitionError::Invalid)?,
        NonZeroU32::new(physical).ok_or(PartitionError::Invalid)?,
        Some(blocks),
    )
    .ok_or(PartitionError::Invalid)?;
    let name = partition_disk_name(parent.name(), number);
    if name.is_empty() {
        return Err(PartitionError::OutOfMemory);
    }
    let driver: Arc<dyn BlockDriver> =
        Arc::new(PartitionDriver::new(Arc::clone(parent), start_lba, blocks));
    let dev = Arc::new(BlockDevice::new(
        BlockDeviceInit {
            name: &name,
            subsystem: parent.subsystem(),
            class: BlockClass::Partition,
            geometry,
            limits: *parent.limits(),
            attributes: parent.attributes(),
            features: parent.features(),
        },
        driver,
        Some(Arc::clone(parent)),
    ));
    Ok(PartitionDevice { dev, number })
}

fn logical_block_size(dev: &Arc<BlockDevice>) -> Result<u32, PartitionError> {
    match dev.control(BlockControlRequest::GetLogicalBlockSize) {
        Ok(BlockControlResponse::U32(size)) if size != 0 && size.is_power_of_two() => Ok(size),
        _ => Err(PartitionError::Io),
    }
}

/// 按字节偏移读取块设备(内部对齐到逻辑块后切分)。
fn read_bytes_at(
    dev: &Arc<BlockDevice>,
    byte_offset: u64,
    out: &mut [u8],
) -> Result<(), PartitionError> {
    if out.is_empty() {
        return Ok(());
    }
    let block_size = logical_block_size(dev)? as usize;
    let in_block = (byte_offset % block_size as u64) as usize;
    let start_lba = byte_offset / block_size as u64;
    let total = in_block
        .checked_add(out.len())
        .ok_or(PartitionError::Invalid)?;
    let block_count = total.div_ceil(block_size);
    let block_count_u32 = u32::try_from(block_count).map_err(|_| PartitionError::Invalid)?;
    let buf_len = block_count
        .checked_mul(block_size)
        .ok_or(PartitionError::Invalid)?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(buf_len)
        .map_err(|_| PartitionError::OutOfMemory)?;
    buf.resize(buf_len, 0);
    dev.submit_bio_wait_borrowed_read(
        BlockRange {
            lba: start_lba,
            blocks: block_count_u32,
        },
        &mut buf,
    )
    .map_err(|_| PartitionError::Io)?;
    out.copy_from_slice(&buf[in_block..in_block + out.len()]);
    Ok(())
}

/// 分区 remap 驱动:把 LBA 偏移加到父设备上后转发 BIO。
struct PartitionDriver {
    parent: Arc<BlockDevice>,
    start_lba: u64,
    blocks: u64,
}

impl PartitionDriver {
    fn new(parent: Arc<BlockDevice>, start_lba: u64, blocks: u64) -> Self {
        Self {
            parent,
            start_lba,
            blocks,
        }
    }
}

impl BlockDriver for PartitionDriver {
    fn queue_bio(&self, mut bio: Bio) -> Result<(), (SubmitError, Bio)> {
        let op = bio.op;
        let range = bio.range;
        // 分区内越界检查(使用 u64 避免块计数回绕)。
        if range.lba > self.blocks || (range.blocks as u64) > self.blocks - range.lba {
            bio.complete(Err(BioIoError::MediaError));
            return Ok(());
        }
        let Some(abs_lba) = range.lba.checked_add(self.start_lba) else {
            bio.complete(Err(BioIoError::MediaError));
            return Ok(());
        };
        let parent_range = BlockRange {
            lba: abs_lba,
            blocks: range.blocks,
        };

        let result = match op {
            BioOp::Read => {
                let Some(scratch) = zeroed_vec(bio.buffer.len()) else {
                    bio.complete(Err(BioIoError::Unavailable));
                    return Ok(());
                };
                match self.parent.submit_bio_wait(
                    BioOp::Read,
                    parent_range,
                    BioBuffer::Owned(scratch.into_boxed_slice()),
                ) {
                    Ok(done) => {
                        if bio.buffer.copy_from_contiguous(done.buffer.as_slice()) {
                            Ok(())
                        } else {
                            Err(BioIoError::MediaError)
                        }
                    }
                    Err(err) => Err(map_partition_bio_error(err)),
                }
            }
            BioOp::Write => {
                let Some(mut scratch) = zeroed_vec(bio.buffer.len()) else {
                    bio.complete(Err(BioIoError::Unavailable));
                    return Ok(());
                };
                if !bio.buffer.copy_to_contiguous(&mut scratch) {
                    bio.complete(Err(BioIoError::MediaError));
                    return Ok(());
                }
                match self.parent.submit_bio_wait(
                    BioOp::Write,
                    parent_range,
                    BioBuffer::Owned(scratch.into_boxed_slice()),
                ) {
                    Ok(_) => Ok(()),
                    Err(err) => Err(map_partition_bio_error(err)),
                }
            }
            BioOp::Flush => match self.parent.submit_bio_wait(
                BioOp::Flush,
                BlockRange { lba: 0, blocks: 0 },
                BioBuffer::None,
            ) {
                Ok(_) => Ok(()),
                Err(err) => Err(map_partition_bio_error(err)),
            },
            BioOp::Discard | BioOp::WriteZeroes => Err(BioIoError::Unsupported),
        };
        bio.complete(result);
        Ok(())
    }

    fn control(
        &self,
        req: BlockControlRequest,
    ) -> Option<Result<BlockControlResponse, ControlError>> {
        // 只读状态跟随父设备(loop 的 read_only 是运行期动态的);容量/块大小由
        // 分区自身的 geometry 由块层默认回答。
        match req {
            BlockControlRequest::GetReadOnly => Some(self.parent.control(req)),
            _ => None,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn zeroed_vec(len: usize) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).ok()?;
    v.resize(len, 0);
    Some(v)
}

fn map_partition_bio_error(err: BioError) -> BioIoError {
    match err {
        BioError::Submit(_) => BioIoError::Unavailable,
        BioError::Io(err) => err,
    }
}
