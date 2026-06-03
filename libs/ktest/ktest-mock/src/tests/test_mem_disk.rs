//! MemDisk 自身行为测试。
//!
//! 验证构造、扇区读写往返、越界拒绝、对齐补齐。

extern crate std;

use crate::MemDisk;
use ktest::ktest;
use std::vec;

#[ktest]
fn new_creates_zeroed_disk() {
    let disk = MemDisk::new(4, 512);
    assert_eq!(disk.sector_size(), 512);
    assert_eq!(disk.sector_count(), 4);
    let data = disk.dump();
    assert_eq!(data.len(), 2048);
    assert!(data.iter().all(|&b| b == 0));
}

#[ktest]
fn write_and_read_roundtrip() {
    let disk = MemDisk::new(4, 512);
    let payload = [0xabu8; 512];
    assert!(disk.write_sectors(1, 1, &payload));
    let mut buf = [0u8; 512];
    assert!(disk.read_sectors(1, 1, &mut buf));
    assert_eq!(buf, payload);
}

#[ktest]
fn lba_out_of_range_returns_false() {
    let disk = MemDisk::new(4, 512);
    assert!(!disk.read_sectors(4, 1, &mut [0u8; 512]));
    assert!(!disk.write_sectors(4, 1, &[0u8; 512]));
}

#[ktest]
fn count_out_of_range_returns_false() {
    let disk = MemDisk::new(4, 512);
    assert!(!disk.read_sectors(2, 3, &mut [0u8; 1536]));
    assert!(!disk.write_sectors(2, 3, &[0u8; 1536]));
}

#[ktest]
fn lba_overflow_returns_false() {
    let disk = MemDisk::new(4, 512);
    assert!(!disk.read_sectors(u64::MAX, 1, &mut [0u8; 512]));
    assert!(!disk.write_sectors(u64::MAX, 1, &[0u8; 512]));
}

#[ktest]
fn buffer_too_short_returns_false() {
    let disk = MemDisk::new(4, 512);
    assert!(!disk.read_sectors(0, 2, &mut [0u8; 512]));
    assert!(!disk.write_sectors(0, 2, &[0u8; 512]));
}

#[ktest]
fn buffer_too_short_no_data_modified() {
    let disk = MemDisk::new(4, 512);
    let original = disk.dump();
    assert!(!disk.write_sectors(0, 2, &[0xabu8; 512]));
    assert_eq!(disk.dump(), original);
}

#[ktest]
fn from_bytes_pads_to_sector_boundary() {
    let data = vec![0xccu8; 600];
    let disk = MemDisk::from_bytes(data, 512);
    assert_eq!(disk.sector_size(), 512);
    assert_eq!(disk.sector_count(), 2);
    let dump = disk.dump();
    assert_eq!(dump.len(), 1024);
    assert_eq!(dump[0..600], [0xccu8; 600]);
    assert!(dump[600..].iter().all(|&b| b == 0));
}

#[ktest]
#[should_panic]
fn sector_size_zero_panics() {
    MemDisk::new(4, 0);
}
