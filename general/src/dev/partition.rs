//! MBR / GPT 分区扫描与分区块设备。
//!
//! 启动期对 `BlockClass::Whole` 块设备扫描分区表，为每个有效分区创建
//! `BlockClass::Partition` 堆叠块设备：BIO 提交时把 LBA 平移父设备偏移后
//! 转发给父设备底层驱动，并在全局设备表中注册独立 function（`/dev` 节点）。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::any::Any;
use core::num::NonZeroU64;

use crate::dev::bio::{Bio, BioError, BioIoError, BioReqError};
use crate::dev::block::{
    BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockGeometry, BlockRange,
    BlockSubmitError,
};
use crate::dev::enumerate::DEVICES;
use crate::dev::function::BlockFunction;
use vfs::sync::Spinlock;

/// MBR 分区表签名。
const MBR_SIGNATURE: u16 = 0xAA55;
/// GPT 头部签名。
const GPT_MAGIC: &[u8; 8] = b"EFI PART";
/// GPT 单个分区条目的固定长度（字节）。
const GPT_ENTRY_SIZE: usize = 128;
/// 单次扫描最多解析的分区条目数。
const GPT_MAX_ENTRIES: usize = 128;

/// 单个分区的静态描述。
#[derive(Clone, Debug)]
pub struct PartitionInfo {
    /// 分区号（1 起，MBR 槽位号或 GPT 条目序号）。
    pub index: u32,
    /// 分区起始 LBA（父设备逻辑块坐标）。
    pub start_lba: u64,
    /// 分区长度（父设备逻辑块数）。
    pub block_count: u64,
    /// GPT 分区名（MBR 无）。
    pub name: Option<Box<str>>,
}

/// 分区堆叠驱动：BIO 偏移后转发给父设备。
struct PartitionDriver {
    parent: Arc<BlockDevice>,
    start_lba: u64,
}

impl BlockDriver for PartitionDriver {
    fn queue_bio(&self, mut bio: Bio) -> Result<(), (BlockSubmitError, Bio)> {
        let Some(lba) = bio.range.lba.checked_add(self.start_lba) else {
            bio.complete(Err(BioIoError::MediaError));
            return Ok(());
        };
        bio.range.lba = lba;
        self.parent.queue_bio_forward(bio)
    }

    fn drain(&self) {
        self.parent.drain();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 已扫描过的 whole 盘名（幂等保护）。
static SCANNED_DISKS: Spinlock<Vec<Box<str>>> = Spinlock::new(Vec::new());

/// 扫描并注册 `disk` 上的所有分区设备。
///
/// 仅接受 `BlockClass::Whole` 设备；重复扫描同一盘会直接返回空。返回本次
/// 成功注册的分区设备列表。
pub fn scan_and_register(disk: &Arc<BlockDevice>) -> Vec<Arc<BlockDevice>> {
    if disk.class() != BlockClass::Whole {
        return Vec::new();
    }
    {
        let mut scanned = SCANNED_DISKS.lock();
        if scanned.iter().any(|name| name.as_ref() == disk.name()) {
            return Vec::new();
        }
        if scanned.try_reserve(1).is_ok() {
            scanned.push(disk.name().into());
        }
    }

    let infos = scan(disk);
    let mut out = Vec::new();
    let disk_name = disk.name();
    let physical = disk.geometry().physical_block_size();
    for info in infos {
        let Some(count) = NonZeroU64::new(info.block_count) else {
            continue;
        };
        let Some(geometry) = BlockGeometry::new(
            disk.geometry().logical_block_size(),
            physical,
            Some(count.get()),
        ) else {
            continue;
        };
        let name = partition_disk_name(disk_name, info.index);
        if name.is_empty() {
            continue;
        }
        let name = name.into_boxed_str();
        let block = Arc::new(BlockDevice::new(
            BlockDeviceInit {
                name: &name,
                subsystem: "partition",
                class: BlockClass::Partition,
                geometry,
                limits: *disk.limits(),
                attributes: disk.attributes(),
                features: disk.features(),
            },
            Arc::new(PartitionDriver {
                parent: Arc::clone(disk),
                start_lba: info.start_lba,
            }),
            Some(Arc::clone(disk)),
        ));
        let function = BlockFunction::with_projection_name_arc(&name, &name, Arc::clone(&block));
        if DEVICES.register_function(function).is_err() {
            continue;
        }
        log::printk!(
            "[partition] {}: {} (lba {} + {} blocks)",
            disk_name,
            name,
            info.start_lba,
            info.block_count
        );
        out.push(block);
    }
    out
}

/// 对所有 active whole 块设备执行分区扫描注册。
///
/// 供启动根选择路径调用；已扫描盘幂等跳过。返回本次注册的分区设备总数。
pub fn scan_and_register_all() -> usize {
    let disks = crate::vfs::device_files::projection::active_block_devices(&DEVICES.functions);
    let mut total = 0usize;
    for disk in disks {
        total = total.saturating_add(scan_and_register(&disk).len());
    }
    total
}

// ── 分区表解析 ──────────────────────────────────────────────────────────

/// 读取盘首逻辑块，覆盖 MBR（LBA 0）与 GPT 头（LBA 1）。
fn read_disk_head(disk: &Arc<BlockDevice>) -> Option<Vec<u8>> {
    let block_size = disk.geometry().logical_block_size().get() as usize;
    let mut buf = alloc::vec![0u8; 2usize.saturating_mul(block_size)];
    read_single_blocks(disk, 0, &mut buf).ok()?;
    Some(buf)
}

/// 逐块读取，兼容 max_blocks_per_io=1 的单块 PIO 驱动（如 jh7110-mmc）。
fn read_single_blocks(
    disk: &Arc<BlockDevice>,
    start_lba: u64,
    buf: &mut [u8],
) -> Result<(), BioError> {
    let block_size = disk.geometry().logical_block_size().get() as usize;
    for (index, chunk) in buf.chunks_mut(block_size).enumerate() {
        let lba = start_lba.checked_add(index as u64).ok_or_else(|| {
            BioError::Submit(BlockSubmitError::InvalidRequest(BioReqError::OutOfBounds))
        })?;
        disk.submit_bio_wait_borrowed_read(BlockRange { lba, blocks: 1 }, chunk)?;
    }
    Ok(())
}

fn is_mbr(buf: &[u8]) -> bool {
    buf.len() >= 512 && u16::from_le_bytes([buf[510], buf[511]]) == MBR_SIGNATURE
}

fn is_gpt(buf: &[u8]) -> bool {
    buf.len() >= 1024 && &buf[512..520] == GPT_MAGIC
}

fn parse_mbr(buf: &[u8]) -> Vec<PartitionInfo> {
    let mut out = Vec::new();
    for slot in 0..4 {
        let entry = &buf[446 + slot * 16..446 + slot * 16 + 16];
        let ptype = entry[4];
        // 跳过空条目、扩展分区容器（EBR 链暂不支持）与 GPT 保护性分区。
        if ptype == 0 || matches!(ptype, 0x05 | 0x0F | 0x85 | 0xEE) {
            continue;
        }
        let start = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as u64;
        let sectors = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as u64;
        if start == 0 || sectors == 0 {
            continue;
        }
        out.push(PartitionInfo {
            index: slot as u32 + 1,
            start_lba: start,
            block_count: sectors,
            name: None,
        });
    }
    out
}

fn parse_gpt(disk: &Arc<BlockDevice>, head: &[u8]) -> Vec<PartitionInfo> {
    let Some(header) = head.get(512..512 + 92) else {
        return Vec::new();
    };
    let entries_lba = u64::from_le_bytes(header[72..80].try_into().expect("gpt header slice"));
    let entry_count = u32::from_le_bytes(header[80..84].try_into().expect("gpt count")) as usize;
    let entry_size =
        u32::from_le_bytes(header[84..88].try_into().expect("gpt entry size")) as usize;
    if entries_lba == 0 || entry_count == 0 || entry_size < GPT_ENTRY_SIZE {
        return Vec::new();
    }
    let entry_count = entry_count.min(GPT_MAX_ENTRIES);
    let entry_bytes = entry_count.saturating_mul(entry_size);
    let block_size = disk.geometry().logical_block_size().get() as usize;
    let blocks = entry_bytes.div_ceil(block_size).max(1);
    let mut entries = alloc::vec![0u8; blocks.saturating_mul(block_size)];
    if read_single_blocks(disk, entries_lba, &mut entries).is_err() {
        log::warning!("[partition] {}: GPT entries read failed", disk.name());
        return Vec::new();
    }

    let disk_blocks = disk.geometry().block_count();
    let mut out = Vec::new();
    for (index, raw) in entries
        .chunks_exact(entry_size)
        .take(entry_count)
        .enumerate()
    {
        if raw[..16].iter().all(|b| *b == 0) {
            continue; // 未使用条目
        }
        let first = u64::from_le_bytes(raw[32..40].try_into().expect("gpt first lba"));
        let last = u64::from_le_bytes(raw[40..48].try_into().expect("gpt last lba"));
        if first == 0 || last < first {
            continue;
        }
        let block_count = last - first + 1;
        if let Some(total) = disk_blocks {
            if last >= total {
                continue;
            }
        }
        let name = gpt_name(&raw[56..128]);
        out.push(PartitionInfo {
            index: index as u32 + 1,
            start_lba: first,
            block_count,
            name,
        });
    }
    out
}

/// 解码 GPT 分区名（UTF-16LE，BMP 字符）。
fn gpt_name(raw: &[u8]) -> Option<Box<str>> {
    let mut units = Vec::new();
    for chunk in raw.chunks_exact(2) {
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    if units.is_empty() {
        return None;
    }
    let mut name = String::new();
    for unit in units {
        let ch = char::from_u32(u32::from(unit)).unwrap_or('�');
        if name.try_reserve(ch.len_utf8()).is_err() {
            break;
        }
        name.push(ch);
    }
    Some(name.into_boxed_str())
}

fn scan(disk: &Arc<BlockDevice>) -> Vec<PartitionInfo> {
    let head = match read_disk_head(disk) {
        Some(head) => head,
        None => {
            log::warning!("[partition] {}: head read failed", disk.name());
            return Vec::new();
        }
    };
    if is_gpt(&head) {
        parse_gpt(disk, &head)
    } else if is_mbr(&head) {
        parse_mbr(&head)
    } else {
        log::warning!(
            "[partition] {}: no partition table (sig={:02x}{:02x} gpt={} mbr={})",
            disk.name(),
            head.get(510).copied().unwrap_or(0),
            head.get(511).copied().unwrap_or(0),
            is_gpt(&head),
            is_mbr(&head)
        );
        Vec::new()
    }
}

// ── /dev 投影查询接口 ─────────────────────────────────────────────────────
//
// 合并自审计整改(devtmpfs 块):投影层需要按父整盘查询已注册分区设备,以生成
// /dev/<disk>p<part> 节点。分区设备由 `scan_and_register_all` 注册进全局表,
// 这里从注册表按 parent 过滤,避免重复扫描磁盘。

/// 已注册的分区设备快照(供 `/dev` 投影层查询)。
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

/// 磁盘名 -> 分区节点名（数字后缀使用 `p` 分隔，例如 `vd0` -> `vd0p1`）。
///
/// 数字索引盘名追加 `p<序号>`，明确分隔盘索引与分区序号（例如 `vd0p1`）；
/// 不带数字索引的自定义盘名直接追加序号。
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
    let _ = core::fmt::write(&mut name, format_args!("{}", number));
    name
}

#[cfg(test)]
mod tests {
    use super::partition_disk_name;

    #[test]
    fn native_storage_names_separate_disk_and_partition_indices() {
        for (disk, expected) in [
            ("vd0", "vd0p1"),
            ("ahci0", "ahci0p1"),
            ("mmc0", "mmc0p1"),
            ("sdio0", "sdio0p1"),
        ] {
            assert_eq!(partition_disk_name(disk, 1), expected);
        }
    }
}

/// 返回一个整盘块设备当前已注册的分区设备(按 parent 过滤全局注册表)。
pub fn partitions_of(parent: &Arc<BlockDevice>) -> Vec<PartitionDevice> {
    if parent.class() != BlockClass::Whole || !parent.is_active() {
        return Vec::new();
    }
    let Some(functions) = DEVICES.functions.try_list() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for func in functions {
        let Some(block) = crate::dev::function::function_as::<BlockFunction>(&*func) else {
            continue;
        };
        let dev = block.dev();
        if dev.class() != BlockClass::Partition || !dev.is_active() {
            continue;
        }
        let Some(disk) = dev.parent() else {
            continue;
        };
        if !core::ptr::eq(Arc::as_ptr(&disk), Arc::as_ptr(parent)) {
            continue;
        }
        // 分区号由名字尾部数字提取(注册名 `<disk>[p]<n>`)。
        let name = dev.name();
        let number = name
            .rsplit(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        if number == 0 {
            continue;
        }
        if out.try_reserve(1).is_err() {
            return out;
        }
        out.push(PartitionDevice { dev, number });
    }
    out
}
