//! KERNEL_ALLOCATOR 集成测试。
//!
//! 在内核 allocator 初始化完成后执行，验证 allocate/deallocate 往返。

extern crate alloc;

use crate::KERNEL_ALLOCATOR;
use crate::request::{AllocationKind, MemoryDomain, MemoryRequest, Zeroing};
use ktest::ktest;

/// 分配 8 字节小对象，验证指针非空且走 slab 路径。
#[ktest]
fn allocate_small() {
    let req = MemoryRequest::new(MemoryDomain::Kernel, 8, 8);
    let record = KERNEL_ALLOCATOR.allocate(req).expect("allocate 8 bytes");
    assert!(record.ptr != 0);
    assert_eq!(record.kind, AllocationKind::Small);
    KERNEL_ALLOCATOR.deallocate(record.ptr).expect("deallocate");
}

/// 分配后释放，双向均返回 Ok。
#[ktest]
fn allocate_deallocate_roundtrip() {
    let req = MemoryRequest::new(MemoryDomain::Kernel, 64, 8);
    let record = KERNEL_ALLOCATOR.allocate(req).expect("allocate 64 bytes");
    KERNEL_ALLOCATOR.deallocate(record.ptr).expect("deallocate");
}

/// 不同尺寸的分配均应成功，每轮即时释放，避免 Vec 额外分配干扰。
#[ktest]
fn allocate_various_sizes() {
    let sizes = [8, 16, 64, 256, 1024, 4096];
    for &size in &sizes {
        let req = MemoryRequest::new(MemoryDomain::Kernel, size, 8);
        let record = KERNEL_ALLOCATOR.allocate(req).expect("allocate");
        assert!(record.ptr != 0);
        KERNEL_ALLOCATOR.deallocate(record.ptr).expect("deallocate");
    }
}

/// Zeroing::Zeroed 分配的内存应全为 0。
#[ktest]
fn allocate_zeroed() {
    let req = MemoryRequest::new(MemoryDomain::Kernel, 64, 8).with_zeroing(Zeroing::Zeroed);
    let record = KERNEL_ALLOCATOR.allocate(req).expect("allocate zeroed");
    let slice = unsafe { core::slice::from_raw_parts(record.ptr as *const u8, 64) };
    assert!(slice.iter().all(|&b| b == 0));
    KERNEL_ALLOCATOR.deallocate(record.ptr).expect("deallocate");
}

/// 释放无效指针应返回错误。
#[ktest]
fn deallocate_invalid() {
    assert!(KERNEL_ALLOCATOR.deallocate(0xDEAD).is_err());
}

/// 大对象（> 2048 字节） kheap 路径。
#[ktest]
fn allocate_large() {
    let req = MemoryRequest::new(MemoryDomain::Kernel, 8192, 8);
    let record = KERNEL_ALLOCATOR.allocate(req).expect("allocate 8K");
    assert_eq!(record.kind, AllocationKind::Large);
    KERNEL_ALLOCATOR.deallocate(record.ptr).expect("deallocate");
}
