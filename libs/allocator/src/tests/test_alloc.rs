//! KERNEL_ALLOCATOR 集成测试。
//!
//! 在内核 allocator 初始化完成后执行，验证 allocate/deallocate 往返。

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};

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

/// resize API 只能作用于逐对象账本中的活跃对象，不能把任意裸地址当作分配起点。
#[ktest]
fn reallocate_rejects_untracked_pointer() {
    let bogus = 0xDEAD_BEEFusize;
    let request = MemoryRequest::new(MemoryDomain::Kernel, 64, 8);
    assert!(!KERNEL_ALLOCATOR.owns_allocation(bogus));
    assert!(
        KERNEL_ALLOCATOR
            .can_reallocate_in_place(bogus, request)
            .is_err()
    );
    assert!(KERNEL_ALLOCATOR.reallocate(bogus, request).is_err());
}

/// 大对象（> 2048 字节） kheap 路径。
#[ktest]
fn allocate_large() {
    let req = MemoryRequest::new(MemoryDomain::Kernel, 8192, 8);
    let record = KERNEL_ALLOCATOR.allocate(req).expect("allocate 8K");
    assert_eq!(record.kind, AllocationKind::Large);
    KERNEL_ALLOCATOR.deallocate(record.ptr).expect("deallocate");
}

/// 公开查询 API 应直接暴露 allocator 账本中的稳定字段。
#[ktest]
fn allocation_query_helpers_report_record_fields() {
    let req = MemoryRequest::new(MemoryDomain::Kernel, 33, 8);
    let record = KERNEL_ALLOCATOR.allocate(req).expect("allocate 33 bytes");

    assert_eq!(
        KERNEL_ALLOCATOR
            .query_tracked_allocation(record.ptr)
            .expect("query tracked record"),
        record
    );
    assert!(KERNEL_ALLOCATOR.owns_allocation(record.ptr));
    assert_eq!(
        KERNEL_ALLOCATOR
            .allocation_size(record.ptr)
            .expect("query size"),
        33
    );
    assert!(
        KERNEL_ALLOCATOR
            .allocation_usable_size(record.ptr)
            .expect("query usable")
            >= 33
    );
    assert_eq!(
        KERNEL_ALLOCATOR
            .allocation_alignment(record.ptr)
            .expect("query alignment"),
        8
    );

    KERNEL_ALLOCATOR
        .deallocate(record.ptr)
        .expect("deallocate queried object");
    assert!(!KERNEL_ALLOCATOR.owns_allocation(record.ptr));
}

/// slab 路径必须同时满足 size class 和调用方对齐要求。
#[ktest]
fn slab_allocation_respects_requested_alignment() {
    let cases = [(80, 64), (150, 128), (600, 512)];
    for &(size, align) in &cases {
        let record = KERNEL_ALLOCATOR
            .allocate(MemoryRequest::new(MemoryDomain::Kernel, size, align))
            .expect("allocate aligned small object");
        assert_eq!(record.kind, AllocationKind::Small);
        assert_eq!(record.ptr & (align - 1), 0);
        assert!(record.usable_size >= size);
        KERNEL_ALLOCATOR
            .deallocate(record.ptr)
            .expect("deallocate aligned small object");
    }
}

/// reallocate 同尺寸等级时应原地更新账本，不移动对象。
#[ktest]
fn reallocate_small_in_place() {
    let req = MemoryRequest::new(MemoryDomain::Kernel, 64, 8);
    let record = KERNEL_ALLOCATOR.allocate(req).expect("allocate 64 bytes");
    assert!(
        KERNEL_ALLOCATOR
            .can_reallocate_in_place(record.ptr, MemoryRequest::new(MemoryDomain::Kernel, 63, 8))
            .expect("query in-place resize")
    );
    assert!(
        !KERNEL_ALLOCATOR
            .can_reallocate_in_place(
                record.ptr,
                MemoryRequest::new(MemoryDomain::Kernel, 4096, 8)
            )
            .expect("query moving resize")
    );
    let resized = KERNEL_ALLOCATOR
        .reallocate(record.ptr, MemoryRequest::new(MemoryDomain::Kernel, 63, 8))
        .expect("reallocate in place");
    assert_eq!(resized.ptr, record.ptr);
    assert_eq!(resized.kind, AllocationKind::Small);
    assert_eq!(resized.size, 63);
    KERNEL_ALLOCATOR
        .deallocate(resized.ptr)
        .expect("deallocate resized");
}

/// GlobalAlloc::realloc 的同 size-class 快路径应只更新账本，不移动对象。
#[ktest]
fn global_realloc_same_class_keeps_pointer_and_record() {
    let old_layout = Layout::from_size_align(64, 8).expect("valid old layout");
    let new_layout = Layout::from_size_align(63, 8).expect("valid new layout");
    let ptr = unsafe { GlobalAlloc::alloc(&KERNEL_ALLOCATOR, old_layout) };
    assert!(!ptr.is_null());

    let bytes = unsafe { core::slice::from_raw_parts_mut(ptr, 64) };
    for (idx, byte) in bytes.iter_mut().enumerate() {
        *byte = 0xA0u8.wrapping_add(idx as u8);
    }

    let new_ptr = unsafe { GlobalAlloc::realloc(&KERNEL_ALLOCATOR, ptr, old_layout, 63) };
    assert_eq!(new_ptr, ptr);
    assert_eq!(
        KERNEL_ALLOCATOR
            .allocation_size(new_ptr as usize)
            .expect("query realloc size"),
        63
    );
    let bytes = unsafe { core::slice::from_raw_parts(new_ptr, 63) };
    for (idx, byte) in bytes.iter().enumerate() {
        assert_eq!(*byte, 0xA0u8.wrapping_add(idx as u8));
    }

    unsafe { GlobalAlloc::dealloc(&KERNEL_ALLOCATOR, new_ptr, new_layout) };
}

/// GlobalAlloc::realloc 跨分配层搬迁时应复用单次账本 probe 返回的旧记录完成复制。
#[ktest]
fn global_realloc_grow_preserves_prefix() {
    let old_layout = Layout::from_size_align(64, 8).expect("valid old layout");
    let new_layout = Layout::from_size_align(4096, 8).expect("valid new layout");
    let ptr = unsafe { GlobalAlloc::alloc(&KERNEL_ALLOCATOR, old_layout) };
    assert!(!ptr.is_null());

    let old = unsafe { core::slice::from_raw_parts_mut(ptr, 64) };
    for (idx, byte) in old.iter_mut().enumerate() {
        *byte = idx as u8;
    }

    let new_ptr = unsafe { GlobalAlloc::realloc(&KERNEL_ALLOCATOR, ptr, old_layout, 4096) };
    assert!(!new_ptr.is_null());
    assert_eq!(
        KERNEL_ALLOCATOR
            .allocation_size(new_ptr as usize)
            .expect("query grown size"),
        4096
    );
    let new = unsafe { core::slice::from_raw_parts(new_ptr, 64) };
    for (idx, byte) in new.iter().enumerate() {
        assert_eq!(*byte, idx as u8);
    }

    unsafe { GlobalAlloc::dealloc(&KERNEL_ALLOCATOR, new_ptr, new_layout) };
}

/// 原地 reallocate 不能只看 usable size，还必须保持新 Layout 的对齐契约。
#[ktest]
fn reallocate_in_place_rejects_misaligned_target() {
    let req = MemoryRequest::new(MemoryDomain::Kernel, 80, 32);
    let mut records = [None; 16];
    let mut target = None;

    for slot in &mut records {
        let record = KERNEL_ALLOCATOR
            .allocate(req)
            .expect("allocate 96-byte class object");
        if record.ptr & (64 - 1) != 0 {
            target = Some(record);
        }
        *slot = Some(record);
    }

    let target = target.expect("at least one object should not be 64-byte aligned");
    assert!(
        !KERNEL_ALLOCATOR
            .can_reallocate_in_place(target.ptr, MemoryRequest::new(MemoryDomain::Kernel, 64, 64))
            .expect("query stricter alignment resize")
    );

    for slot in records.iter_mut().rev() {
        if let Some(record) = slot.take() {
            KERNEL_ALLOCATOR
                .deallocate(record.ptr)
                .expect("deallocate alignment test object");
        }
    }
}

/// reallocate 跨 slab/kheap 边界时应复制旧内容，并按 Zeroed 请求清零新增区域。
#[ktest]
fn reallocate_small_to_large_preserves_prefix() {
    let req = MemoryRequest::new(MemoryDomain::Kernel, 64, 8);
    let record = KERNEL_ALLOCATOR.allocate(req).expect("allocate 64 bytes");
    let old = unsafe { core::slice::from_raw_parts_mut(record.ptr as *mut u8, 64) };
    for (idx, byte) in old.iter_mut().enumerate() {
        *byte = idx as u8;
    }

    let grown = KERNEL_ALLOCATOR
        .reallocate(
            record.ptr,
            MemoryRequest::new(MemoryDomain::Kernel, 4096, 8).with_zeroing(Zeroing::Zeroed),
        )
        .expect("reallocate to 4K");
    assert_eq!(grown.kind, AllocationKind::Large);
    let bytes = unsafe { core::slice::from_raw_parts(grown.ptr as *const u8, 128) };
    for idx in 0..64 {
        assert_eq!(bytes[idx], idx as u8);
    }
    assert!(bytes[64..128].iter().all(|&byte| byte == 0));
    KERNEL_ALLOCATOR
        .deallocate(grown.ptr)
        .expect("deallocate grown");
}

/// 注册表节点按批补货后，仍应能稳定跟踪超过单批数量的活跃分配。
#[ktest]
fn registry_tracks_many_live_small_objects() {
    const COUNT: usize = 160;
    let req = MemoryRequest::new(MemoryDomain::Kernel, 32, 8);
    let mut records = [None; COUNT];

    for slot in &mut records {
        let record = KERNEL_ALLOCATOR
            .allocate(req)
            .expect("allocate tracked object");
        assert_eq!(
            KERNEL_ALLOCATOR
                .allocation_kind(record.ptr)
                .expect("query kind"),
            AllocationKind::Small
        );
        *slot = Some(record);
    }

    let stats = KERNEL_ALLOCATOR.registry_stats();
    assert!(stats.shard_count > 1);
    assert!(stats.live_small >= COUNT);
    assert!(stats.max_shard_live_records > 0);
    assert!(stats.max_shard_live_records <= stats.live_records);
    assert!(stats.nodes_allocated >= stats.live_records);
    assert!(stats.node_refills > 0);

    for slot in records.iter_mut().rev() {
        let record = slot.take().expect("record exists");
        KERNEL_ALLOCATOR
            .deallocate(record.ptr)
            .expect("deallocate tracked object");
    }

    let stats = KERNEL_ALLOCATOR.registry_stats();
    assert!(stats.free_nodes > 0);
    assert!(stats.nodes_allocated >= stats.free_nodes);
}

/// Buddy 元数据应来自物理内存 carve-out，并在启动后拥有完整节点池。
#[ktest]
fn buddy_metadata_pool_initialized() {
    let stats = KERNEL_ALLOCATOR.buddy_stats();
    assert!(stats.total_pages > 0);
    assert!(stats.metadata_pages > 0);
    assert!(stats.node_capacity >= stats.total_pages);
    assert!(stats.node_used > 0);

    let metadata = KERNEL_ALLOCATOR.metadata_stats();
    assert!(metadata.backing_pages > 0);
    assert!(metadata.dynamic_allocations > 0);
}
