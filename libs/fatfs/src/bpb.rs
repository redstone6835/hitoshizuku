//! FAT BIOS 参数块(BPB)解析。
//!
//! 本模块从引导扇区提取 BPB 字段并按 Microsoft 规范判别 FAT12/16/32 变体。
//! 不做写入:mkfs 不在本驱动的责任范围。

use alloc::vec;

use crate::state::{BlockBackend, BlockBackendError};

/// 判别出的 FAT 变体。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FatKind {
    Fat12,
    Fat16,
    Fat32,
}

/// 从引导扇区解析出的规范化几何信息。
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct BpbInfo {
    pub kind: FatKind,
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub reserved_sectors: u32,
    pub num_fats: u32,
    pub root_entries: u32,
    pub fat_size_sectors: u32,
    pub total_sectors: u32,
    pub root_dir_sectors: u32,
    pub first_data_sector: u32,
    pub total_clusters: u32,
    pub root_cluster: u32,
    pub fs_info_sector: u32,
}

/// 从设备读取引导扇区并解析 BPB。
///
/// 失败原因(EINVAL 等价)由 [`BlockBackendError`] 携带:
/// - 引导扇区签名错误;
/// - 字段不一致(0 簇尺寸、0 FAT 数等);
/// - 类型识别失败(总簇数落在 FAT12/16/32 边界之外的灰色地带);
/// - 整型溢出或字段超出合理范围。
pub(crate) fn parse(backend: &dyn BlockBackend) -> Result<BpbInfo, BlockBackendError> {
    let sector_size = backend.sector_size();
    if sector_size < 512 || sector_size > 4096 || !sector_size.is_power_of_two() {
        return Err(BlockBackendError::OutOfRange);
    }
    let mut boot = vec![0u8; sector_size as usize];
    backend.read_sectors(0, 1, &mut boot)?;
    if boot[510] != 0x55 || boot[511] != 0xaa {
        return Err(BlockBackendError::OutOfRange);
    }
    let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]) as u32;
    let sectors_per_cluster = boot[13] as u32;
    let reserved_sectors = u16::from_le_bytes([boot[14], boot[15]]) as u32;
    let num_fats = boot[16] as u32;
    let root_entries = u16::from_le_bytes([boot[17], boot[18]]) as u32;
    let total_sectors_16 = u16::from_le_bytes([boot[19], boot[20]]) as u32;
    let fat_size_16 = u16::from_le_bytes([boot[22], boot[23]]) as u32;
    let total_sectors_32 = u32::from_le_bytes([boot[32], boot[33], boot[34], boot[35]]);

    if bytes_per_sector < 512
        || bytes_per_sector > 4096
        || !bytes_per_sector.is_power_of_two()
        || bytes_per_sector != sector_size
        || sectors_per_cluster == 0
        || sectors_per_cluster > 128
        || !sectors_per_cluster.is_power_of_two()
        || num_fats == 0
        || num_fats > 4
        || reserved_sectors == 0
    {
        return Err(BlockBackendError::OutOfRange);
    }

    let (fat_size_sectors, root_cluster, fs_info_sector) = if fat_size_16 != 0 {
        (fat_size_16, 0, 0)
    } else {
        let fat_size_32 = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]);
        let root_cluster = u32::from_le_bytes([boot[44], boot[45], boot[46], boot[47]]);
        let fs_info = u16::from_le_bytes([boot[48], boot[49]]) as u32;
        (fat_size_32, root_cluster, fs_info)
    };
    if fat_size_sectors == 0 {
        return Err(BlockBackendError::OutOfRange);
    }
    let total_sectors = if total_sectors_16 != 0 {
        total_sectors_16
    } else {
        total_sectors_32
    };
    if total_sectors == 0 || total_sectors > 0x0FFF_FFFF {
        return Err(BlockBackendError::OutOfRange);
    }

    let root_dir_sectors = root_entries
        .checked_mul(32)
        .and_then(|v| v.checked_add(bytes_per_sector - 1))
        .map(|v| v / bytes_per_sector)
        .ok_or(BlockBackendError::OutOfRange)?;

    let fat_region_sectors = num_fats
        .checked_mul(fat_size_sectors)
        .ok_or(BlockBackendError::OutOfRange)?;

    let overhead = reserved_sectors
        .checked_add(fat_region_sectors)
        .and_then(|v| v.checked_add(root_dir_sectors))
        .ok_or(BlockBackendError::OutOfRange)?;

    if overhead >= total_sectors {
        return Err(BlockBackendError::OutOfRange);
    }
    let first_data_sector = overhead;
    let data_sectors = total_sectors - first_data_sector;
    let total_clusters = data_sectors / sectors_per_cluster;

    if total_clusters < 2 {
        return Err(BlockBackendError::OutOfRange);
    }

    let kind = if total_clusters < 4085 {
        FatKind::Fat12
    } else if total_clusters < 65525 {
        FatKind::Fat16
    } else {
        FatKind::Fat32
    };
    if kind == FatKind::Fat32 && root_cluster < 2 {
        return Err(BlockBackendError::OutOfRange);
    }

    Ok(BpbInfo {
        kind,
        bytes_per_sector,
        sectors_per_cluster,
        reserved_sectors,
        num_fats,
        root_entries,
        fat_size_sectors,
        total_sectors,
        root_dir_sectors,
        first_data_sector,
        total_clusters,
        root_cluster,
        fs_info_sector,
    })
}
