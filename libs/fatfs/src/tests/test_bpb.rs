//! FAT BPB 解析测试。

extern crate std;

use ktest::ktest;
use ktest_mock::MemDisk;
use std::sync::Arc;
use std::vec;
use std::vec::Vec;
use crate::bpb;

fn boot_sector(bytes_per_sector: u16, sec_per_cluster: u8, num_fats: u8,
               root_entries: u16, total_sec_16: u16, fat_size_16: u16,
               total_sec_32: u32, fat_size_32: u32, root_cluster: u32,
               sector_size: u32) -> Vec<u8> {
    let mut b = vec![0u8; sector_size as usize];
    b[11..13].copy_from_slice(&bytes_per_sector.to_le_bytes());
    b[13] = sec_per_cluster;
    b[14..16].copy_from_slice(&1u16.to_le_bytes());      // reserved_sectors = 1
    b[16] = num_fats;
    b[17..19].copy_from_slice(&root_entries.to_le_bytes());
    b[19..21].copy_from_slice(&total_sec_16.to_le_bytes());
    b[21] = 0xf8;                                        // media descriptor
    b[22..24].copy_from_slice(&fat_size_16.to_le_bytes());
    b[32..36].copy_from_slice(&total_sec_32.to_le_bytes());
    b[36..40].copy_from_slice(&fat_size_32.to_le_bytes());
    b[44..48].copy_from_slice(&root_cluster.to_le_bytes());
    b[48..50].copy_from_slice(&1u16.to_le_bytes());      // fs_info_sector
    b[510] = 0x55;
    b[511] = 0xaa;
    b
}

/// 构造 FAT32 首扇区，验证 BPB 基本字段（扇区大小、簇大小、FAT 数、根簇号）。
#[ktest]
fn parse_bpb_fields() {
    let sector = boot_sector(512, 1, 2, 0, 0, 0, 200000, 128, 2, 512);
    let disk = Arc::new(MemDisk::from_bytes(sector, 512));
    let info = bpb::parse(&*disk).expect("parse BPB");
    assert_eq!(info.bytes_per_sector, 512);
    assert_eq!(info.sectors_per_cluster, 1);
    assert_eq!(info.num_fats, 2);
    assert_eq!(info.root_cluster, 2);
}

/// 构造 FAT12/16 风格 BPB，验证 root_entries 和 fat_size_sectors 字段。
#[ktest]
fn parse_bpb_root_entries() {
    let sector = boot_sector(512, 2, 2, 512, 4096, 32, 0, 0, 0, 512);
    let disk = Arc::new(MemDisk::from_bytes(sector, 512));
    let info = bpb::parse(&*disk).expect("parse BPB");
    assert_eq!(info.fat_size_sectors, 32);
    assert_eq!(info.root_entries, 512);
}

/// 启动签名（0xAA55）不匹配时返回错误。
#[ktest]
fn reject_invalid_signature() {
    let mut sector = boot_sector(512, 1, 2, 0, 0, 0, 200000, 128, 2, 512);
    sector[510] = 0;
    let disk = Arc::new(MemDisk::from_bytes(sector, 512));
    assert!(bpb::parse(&*disk).is_err());
}
