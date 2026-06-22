//! FAT 表操作测试。

extern crate std;

use crate::bpb::FatKind;
use crate::fat::FatTable;
use ktest::ktest;
use ktest_mock::MemDisk;
use std::sync::Arc;
use std::vec;

/// 构造 FAT 表测试盘：预留扇区 0（引导扇区），从扇区 1 起为 FAT 数据。
/// `entries` 为按 cluster 编号索引的 FAT 条目数组，索引 0/1 保留未用。
fn make_fat_disk(entries: &[u32], kind: FatKind) -> (Arc<MemDisk>, FatTable) {
    let entry_size: u32 = match kind {
        FatKind::Fat32 => 4,
        _ => 2,
    };
    let total_clusters = entries.len() as u32;
    let data_len = total_clusters * entry_size;
    let sectors_needed = (data_len + 511) / 512;
    let mut data = vec![0u8; 512];
    data.resize(512 + sectors_needed as usize * 512, 0);
    for (cluster, &v) in entries.iter().enumerate() {
        let off = 512 + cluster * entry_size as usize;
        match kind {
            FatKind::Fat32 => data[off..off + 4].copy_from_slice(&v.to_le_bytes()),
            _ => data[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes()),
        }
    }
    let disk = Arc::new(MemDisk::from_bytes(data, 512));
    let table = FatTable::new(kind, 1, sectors_needed, 1, 512, total_clusters, 16, 2);
    (disk, table)
}

/// get 从 FAT 表中读取指定簇号的值，validate_cluster 检查簇号范围。
#[ktest]
fn get_cluster_value() {
    let (disk, table) = make_fat_disk(&[0, 0, 3, 4, 0x0ffffff8], FatKind::Fat32);
    let v = table.get(&*disk, 2).expect("get cluster 2");
    assert_eq!(v, 3);
}

/// next_cluster 返回簇链中的下一个簇号，到达 EOC 返回 None。
#[ktest]
fn next_cluster_linear() {
    let (disk, table) = make_fat_disk(&[0, 0, 3, 4, 0x0ffffff8], FatKind::Fat32);
    let n = table.next_cluster(&*disk, 2).expect("next");
    assert_eq!(n, Some(3));
}

/// is_eoc 对大于等于 EOC 标记的值返回 true，正常簇号返回 false。
#[ktest]
fn is_eoc_marker() {
    let table = FatTable::new(FatKind::Fat32, 1, 1, 1, 512, 100, 16, 2);
    assert!(table.is_eoc(0x0ffffff8));
    assert!(!table.is_eoc(5));
}

/// walk_chain 从起始簇走 N 步，返回最终簇号。
#[ktest]
fn walk_chain_n_steps() {
    let (disk, table) = make_fat_disk(&[0, 0, 3, 4, 0x0ffffff8], FatKind::Fat32);
    let end = table.walk_chain(&*disk, 2, 2).expect("walk");
    assert_eq!(end, Some(4));
}

/// alloc_cluster_run 不应强制要求物理连续簇；FAT 链可以把多个空闲 run 串起来。
#[ktest]
fn alloc_cluster_run_links_fragmented_runs() {
    let entries = [
        0, 0, 0, 0, 0x0ffffff8, // 占用簇 4，把空闲区切成 2..3 和 5..6 两段
        0, 0, 0x0ffffff8,
    ];
    let (disk, table) = make_fat_disk(&entries, FatKind::Fat32);

    let (first, last) = table
        .alloc_cluster_run(&*disk, None, 4)
        .expect("allocate fragmented run");

    assert_eq!((first, last), (2, 6));
    assert_eq!(table.get(&*disk, 2).expect("cluster 2"), 3);
    assert_eq!(table.get(&*disk, 3).expect("cluster 3"), 5);
    assert_eq!(table.get(&*disk, 5).expect("cluster 5"), 6);
    assert!(table.is_eoc(table.get(&*disk, 6).expect("cluster 6")));
}

/// FAT16 使用 2 字节表项，批量扫描和写链必须按类型宽度前进。
#[ktest]
fn alloc_cluster_run_uses_fat16_entry_width() {
    let entries = [0, 0, 0, 0, 0xfff8, 0, 0, 0xfff8];
    let (disk, table) = make_fat_disk(&entries, FatKind::Fat16);

    let (first, last) = table
        .alloc_cluster_run(&*disk, None, 4)
        .expect("allocate fat16 fragmented run");

    assert_eq!((first, last), (2, 6));
    assert_eq!(table.get(&*disk, 2).expect("cluster 2"), 3);
    assert_eq!(table.get(&*disk, 3).expect("cluster 3"), 5);
    assert_eq!(table.get(&*disk, 5).expect("cluster 5"), 6);
    assert!(table.is_eoc(table.get(&*disk, 6).expect("cluster 6")));
}

/// free_chain 需要按 FAT 链而不是物理连续区间释放，碎片链也要全部清零。
#[ktest]
fn free_chain_clears_fragmented_fat32_chain() {
    let entries = [
        0, 0, 3, 5, 0x0ffffff8, // 簇 4 是无关占用块，不能被释放
        6, 0x0ffffff8,
    ];
    let (disk, table) = make_fat_disk(&entries, FatKind::Fat32);

    let freed = table.free_chain(&*disk, 2).expect("free fragmented chain");

    assert_eq!(freed, 4);
    for cluster in [2, 3, 5, 6] {
        assert_eq!(table.get(&*disk, cluster).expect("released cluster"), 0);
    }
    assert!(table.is_eoc(table.get(&*disk, 4).expect("unrelated cluster")));
}

/// FAT16 快速释放路径必须使用 2 字节表项宽度。
#[ktest]
fn free_chain_uses_fat16_entry_width() {
    let entries = [0, 0, 3, 5, 0xfff8, 6, 0xfff8];
    let (disk, table) = make_fat_disk(&entries, FatKind::Fat16);

    let freed = table.free_chain(&*disk, 2).expect("free fat16 chain");

    assert_eq!(freed, 4);
    for cluster in [2, 3, 5, 6] {
        assert_eq!(table.get(&*disk, cluster).expect("released cluster"), 0);
    }
    assert!(table.is_eoc(table.get(&*disk, 4).expect("unrelated cluster")));
}
