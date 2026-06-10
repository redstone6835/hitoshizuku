//! KERNEL_ALLOCATOR 集成测试。
//!
//! 在内核 allocator 初始化完成后执行，验证 allocate/deallocate 往返。

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};

use crate::error::DeallocationError;
use crate::request::{
    AllocationKind, MemoryDomain, MemoryPlacement, MemoryRequest, PagePolicy, PhysicalAllocRequest,
    Zeroing,
};
use crate::{
    AllocationError, AllocationRegistryAuditFlags, AllocatorAuditFlags, KERNEL_ALLOCATOR,
    ManagedAllocFlags, PAGE_SIZE, PhysicalAllocation, PhysicalFreeError,
};
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

/// typed allocator API 必须在入口处拒绝非法请求，不能把 size/align 静默改写后写入账本。
#[ktest]
fn allocation_rejects_invalid_request_before_registry_update() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    assert_eq!(
        KERNEL_ALLOCATOR.allocate(MemoryRequest::new(MemoryDomain::Kernel, 0, 8)),
        Err(AllocationError::InvalidLayout)
    );
    assert_eq!(
        KERNEL_ALLOCATOR.allocate(MemoryRequest::new(MemoryDomain::Kernel, 8, 0)),
        Err(AllocationError::InvalidLayout)
    );
    assert_eq!(
        KERNEL_ALLOCATOR.allocate(MemoryRequest::new(MemoryDomain::Kernel, 8, 3)),
        Err(AllocationError::InvalidLayout)
    );

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
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

/// kheap 热路径应复用最近释放的基础页映射，但缓存项不能进入 registry 活跃账本。
#[ktest]
fn kheap_reuses_cached_base_ranges_without_registry_leak() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    let kheap_before = KERNEL_ALLOCATOR.layer_stats().kheap;

    let first = KERNEL_ALLOCATOR
        .allocate(MemoryRequest::new(
            MemoryDomain::Kernel,
            PAGE_SIZE,
            PAGE_SIZE,
        ))
        .expect("allocate cacheable kheap range");
    assert_eq!(first.kind, AllocationKind::Large);
    KERNEL_ALLOCATOR
        .deallocate(first.ptr)
        .expect("cache first kheap range");

    let after_free = KERNEL_ALLOCATOR.layer_stats().kheap;
    assert!(after_free.cache_inserts >= kheap_before.cache_inserts + 1);
    assert!(after_free.cached_pages >= kheap_before.cached_pages + 1);

    let cached = KERNEL_ALLOCATOR.audit();
    assert!(cached.is_consistent());
    assert_eq!(cached.registry_live_records, before.registry_live_records);
    assert_eq!(cached.kheap_active_allocs, before.kheap_active_allocs);

    let second = KERNEL_ALLOCATOR
        .allocate(MemoryRequest::new(
            MemoryDomain::Kernel,
            PAGE_SIZE,
            PAGE_SIZE,
        ))
        .expect("reuse cached kheap range");
    assert_eq!(second.kind, AllocationKind::Large);
    let after_reuse = KERNEL_ALLOCATOR.layer_stats().kheap;
    assert!(after_reuse.cache_hits >= after_free.cache_hits + 1);

    KERNEL_ALLOCATOR
        .deallocate(second.ptr)
        .expect("deallocate reused kheap range");

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.kheap_active_allocs, before.kheap_active_allocs);
}

/// 大页语义不能混入基础页缓存；未来真正启用大页映射后仍需要按策略隔离后端资源。
#[ktest]
fn kheap_large_page_policy_bypasses_base_range_cache() {
    let before = KERNEL_ALLOCATOR.layer_stats().kheap;
    let record = KERNEL_ALLOCATOR
        .allocate(
            MemoryRequest::new(MemoryDomain::Kernel, PAGE_SIZE, PAGE_SIZE)
                .with_page_policy(PagePolicy::RequireLarge),
        )
        .expect("allocate require-large kheap range");
    assert_eq!(record.kind, AllocationKind::Large);
    assert!(record.order >= 9);

    KERNEL_ALLOCATOR
        .deallocate(record.ptr)
        .expect("deallocate require-large kheap range");
    let after = KERNEL_ALLOCATOR.layer_stats().kheap;
    assert_eq!(after.cache_inserts, before.cache_inserts);

    let audit = KERNEL_ALLOCATOR.audit();
    assert!(audit.is_consistent());
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

/// small record 必须携带 slab node cookie，释放路径才能绕过 slab 链表扫描。
#[ktest]
fn slab_record_carries_private_backend_cookie() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    let stats_before = KERNEL_ALLOCATOR.hotspot_summary();

    let record = KERNEL_ALLOCATOR
        .allocate(MemoryRequest::new(MemoryDomain::Kernel, 96, 8))
        .expect("allocate small object with backend cookie");
    assert_eq!(record.kind, AllocationKind::Small);
    assert!(record.backend_cookie != 0);

    let queried = KERNEL_ALLOCATOR
        .query_tracked_allocation(record.ptr)
        .expect("query tracked small allocation");
    assert_eq!(queried, record);
    let debug = alloc::format!("{:?}", queried);
    assert!(!debug.contains("backend_cookie"));

    KERNEL_ALLOCATOR
        .deallocate(record.ptr)
        .expect("deallocate cookie-backed small object");
    let slab_after = KERNEL_ALLOCATOR.layer_stats().slab;
    assert!(slab_after.fast_free_hits > 0);
    assert_eq!(
        KERNEL_ALLOCATOR.hotspot_summary().slab_fast_free_fallbacks,
        stats_before.slab_fast_free_fallbacks
    );

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.slab_live_records, before.slab_live_records);
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

    let stats_before = KERNEL_ALLOCATOR.stats();
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
    let stats_after = KERNEL_ALLOCATOR.stats();
    assert_eq!(stats_after.total_reallocs, stats_before.total_reallocs + 1);
    assert_eq!(stats_after.total_allocs, stats_before.total_allocs);
    assert_eq!(stats_after.total_deallocs, stats_before.total_deallocs);
    assert_eq!(
        stats_after.total_bytes_allocated,
        stats_before.total_bytes_allocated
    );
    assert_eq!(
        stats_after.total_bytes_freed,
        stats_before.total_bytes_freed
    );

    unsafe { GlobalAlloc::dealloc(&KERNEL_ALLOCATOR, new_ptr, new_layout) };
}

/// GlobalAlloc::realloc 跨分配层搬迁时应复用单次账本 probe 返回的旧记录完成复制。
#[ktest]
fn global_realloc_grow_preserves_prefix() {
    let old_layout = Layout::from_size_align(64, 8).expect("valid old layout");
    let new_layout = Layout::from_size_align(4096, 8).expect("valid new layout");
    let ptr = unsafe { GlobalAlloc::alloc(&KERNEL_ALLOCATOR, old_layout) };
    assert!(!ptr.is_null());
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    let stats_after_alloc = KERNEL_ALLOCATOR.stats();

    let old = unsafe { core::slice::from_raw_parts_mut(ptr, 64) };
    for (idx, byte) in old.iter_mut().enumerate() {
        *byte = idx as u8;
    }

    let new_ptr = unsafe { GlobalAlloc::realloc(&KERNEL_ALLOCATOR, ptr, old_layout, 4096) };
    assert!(!new_ptr.is_null());
    assert!(!KERNEL_ALLOCATOR.owns_allocation(ptr as usize));
    assert!(KERNEL_ALLOCATOR.owns_allocation(new_ptr as usize));
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

    let moved = KERNEL_ALLOCATOR.audit();
    assert!(moved.is_consistent());
    assert_eq!(moved.registry_live_records, before.registry_live_records);
    assert_eq!(moved.slab_live_records + 1, before.slab_live_records);
    assert_eq!(moved.kheap_live_records, before.kheap_live_records + 1);

    let stats_after_realloc = KERNEL_ALLOCATOR.stats();
    assert_eq!(
        stats_after_realloc.total_reallocs,
        stats_after_alloc.total_reallocs + 1
    );
    assert_eq!(
        stats_after_realloc.total_allocs,
        stats_after_alloc.total_allocs + 1
    );
    assert_eq!(
        stats_after_realloc.total_deallocs,
        stats_after_alloc.total_deallocs + 1
    );
    assert_eq!(
        stats_after_realloc.total_bytes_allocated,
        stats_after_alloc.total_bytes_allocated + new_layout.size() as u64
    );
    assert_eq!(
        stats_after_realloc.total_bytes_freed,
        stats_after_alloc.total_bytes_freed + old_layout.size() as u64
    );

    unsafe { GlobalAlloc::dealloc(&KERNEL_ALLOCATOR, new_ptr, new_layout) };

    let stats_after_free = KERNEL_ALLOCATOR.stats();
    assert_eq!(
        stats_after_free.total_deallocs,
        stats_after_alloc.total_deallocs + 2
    );
    assert_eq!(
        stats_after_free.total_bytes_freed,
        stats_after_alloc.total_bytes_freed + old_layout.size() as u64 + new_layout.size() as u64
    );
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
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
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
    assert!(!KERNEL_ALLOCATOR.owns_allocation(record.ptr));
    assert!(KERNEL_ALLOCATOR.owns_allocation(grown.ptr));
    let bytes = unsafe { core::slice::from_raw_parts(grown.ptr as *const u8, 128) };
    for idx in 0..64 {
        assert_eq!(bytes[idx], idx as u8);
    }
    assert!(bytes[64..128].iter().all(|&byte| byte == 0));

    let moved = KERNEL_ALLOCATOR.audit();
    assert!(moved.is_consistent());
    assert_eq!(moved.registry_live_records, before.registry_live_records);
    assert_eq!(moved.slab_live_records + 1, before.slab_live_records);
    assert_eq!(moved.kheap_live_records, before.kheap_live_records + 1);

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
    assert!(stats.nonempty_buckets > 0);
    assert!(stats.nonempty_buckets <= stats.live_records);
    assert!(stats.max_shard_nonempty_buckets <= stats.nonempty_buckets);
    assert!(stats.nodes_allocated >= stats.live_records);
    assert!(stats.node_refills > 0);

    let audit = KERNEL_ALLOCATOR.registry_audit();
    assert_eq!(audit.flags, AllocationRegistryAuditFlags::empty());
    assert_eq!(audit.scanned_live_records, stats.live_records);
    assert_eq!(audit.scanned_live_small, stats.live_small);
    assert_eq!(audit.scanned_nonempty_buckets, stats.nonempty_buckets);
    assert_eq!(audit.scanned_max_chain_len, stats.max_chain_len);

    let snapshot = KERNEL_ALLOCATOR.registry_snapshot();
    assert_eq!(snapshot.audit.flags, AllocationRegistryAuditFlags::empty());
    assert_eq!(
        snapshot.audit.scanned_live_records,
        snapshot.stats.live_records
    );
    assert_eq!(snapshot.audit.scanned_live_small, snapshot.stats.live_small);
    assert_eq!(snapshot.audit.scanned_free_nodes, snapshot.stats.free_nodes);
    assert_eq!(
        snapshot.audit.scanned_nonempty_buckets,
        snapshot.stats.nonempty_buckets
    );

    for slot in records.iter_mut().rev() {
        let record = slot.take().expect("record exists");
        KERNEL_ALLOCATOR
            .deallocate(record.ptr)
            .expect("deallocate tracked object");
    }

    let stats = KERNEL_ALLOCATOR.registry_stats();
    assert!(stats.free_nodes > 0);
    assert!(stats.nodes_allocated >= stats.free_nodes);
    assert!(stats.nonempty_buckets <= stats.live_records);

    let audit = KERNEL_ALLOCATOR.registry_audit();
    assert_eq!(audit.flags, AllocationRegistryAuditFlags::empty());
    assert_eq!(audit.scanned_live_records, stats.live_records);
    assert_eq!(audit.scanned_free_nodes, stats.free_nodes);
    assert_eq!(audit.scanned_nonempty_buckets, stats.nonempty_buckets);

    let snapshot = KERNEL_ALLOCATOR.registry_snapshot();
    assert_eq!(snapshot.audit.flags, AllocationRegistryAuditFlags::empty());
    assert_eq!(
        snapshot.audit.scanned_live_records,
        snapshot.stats.live_records
    );
    assert_eq!(snapshot.audit.scanned_free_nodes, snapshot.stats.free_nodes);
    assert_eq!(
        snapshot.audit.scanned_nonempty_buckets,
        snapshot.stats.nonempty_buckets
    );
}

/// 审计接口应能把 registry 和后端计数稳定对齐。
#[ktest]
fn allocator_audit_reports_consistent_layer_accounting() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    assert_eq!(before.flags, AllocatorAuditFlags::empty());
    assert_eq!(
        before.registry_structure.flags,
        AllocationRegistryAuditFlags::empty()
    );

    let small = KERNEL_ALLOCATOR
        .allocate(MemoryRequest::new(MemoryDomain::Kernel, 32, 8))
        .expect("allocate audited small object");
    let large = KERNEL_ALLOCATOR
        .allocate(MemoryRequest::new(MemoryDomain::Kernel, 4096, 8))
        .expect("allocate audited large object");

    let during = KERNEL_ALLOCATOR.audit();
    assert!(during.is_consistent());
    assert_eq!(
        during.registry_structure.scanned_live_records,
        during.registry_live_records
    );
    assert_eq!(
        during.registry_structure.scanned_free_nodes,
        during.registry_node_capacity - during.registry_live_records
    );
    assert_eq!(
        during.registry_live_records,
        before.registry_live_records + 2
    );
    assert_eq!(during.slab_active_objects, before.slab_active_objects + 1);
    assert_eq!(during.slab_live_records, before.slab_live_records + 1);
    assert_eq!(during.kheap_active_allocs, before.kheap_active_allocs + 1);
    assert_eq!(during.kheap_live_records, before.kheap_live_records + 1);
    assert_eq!(during.registry_boot_records, before.registry_boot_records);
    assert_eq!(
        during.registry_physical_records,
        before.registry_physical_records
    );
    assert_eq!(
        during.registry_nodes_accounted,
        during.registry_node_capacity
    );
    assert_eq!(
        during
            .registry_structure
            .scanned_live_records
            .saturating_add(during.registry_structure.scanned_free_nodes),
        during.registry_node_capacity
    );
    assert!(during.slab_active_bytes <= during.slab_backing_bytes);
    assert_eq!(during.kheap_active_bytes, during.kheap_page_bytes);

    KERNEL_ALLOCATOR
        .deallocate(small.ptr)
        .expect("deallocate audited small object");
    KERNEL_ALLOCATOR
        .deallocate(large.ptr)
        .expect("deallocate audited large object");

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.slab_active_objects, before.slab_active_objects);
    assert_eq!(after.slab_live_records, before.slab_live_records);
    assert_eq!(after.kheap_active_allocs, before.kheap_active_allocs);
    assert_eq!(after.kheap_live_records, before.kheap_live_records);
    assert_eq!(after.registry_boot_records, before.registry_boot_records);
    assert_eq!(
        after.registry_physical_records,
        before.registry_physical_records
    );
}

/// 诊断文本应包含 audit 行，并且格式化自身不改变 allocator 账本状态。
#[ktest]
fn allocator_diagnostic_reports_audit_snapshot() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    let mut buf = [0u8; 2048];
    let len = KERNEL_ALLOCATOR.format_diagnostic(&mut buf);
    assert!(len > 0);
    let text = core::str::from_utf8(&buf[..len]).expect("diagnostic is ascii");
    assert!(text.contains("Audit: ok=1"));
    assert!(text.contains("reg_struct=0"));
    assert!(text.contains("Hot: slab_hit="));
    assert!(text.contains("boot="));
    assert!(text.contains("physrec="));

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.registry_node_capacity, before.registry_node_capacity);
}

/// 热点摘要是派生型诊断接口，读取它不能改变 allocator 账本。
#[ktest]
fn allocator_hotspot_summary_is_no_alloc_snapshot() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    let hot = KERNEL_ALLOCATOR.hotspot_summary();
    assert!(hot.slab_cache_hit_per_mille <= 1000);
    assert!(hot.slab_cache_miss_per_mille <= 1000);
    assert!(hot.registry_live_per_bucket_per_mille <= 1000);
    assert!(hot.registry_nonempty_load_per_mille <= 1000);
    assert!(hot.registry_max_shard_nonempty_buckets <= hot.registry_nonempty_buckets);
    assert!(hot.kheap_failure_per_mille <= 1000);
    assert!(hot.kheap_realloc_per_mille <= 1000);
    assert!(hot.kernel_vmem_largest_free_percent <= 100);
    assert!(hot.managed_fragmentation_per_mille <= 1000);

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.registry_node_capacity, before.registry_node_capacity);
}

/// managed 对象拒绝释放时，registry 必须回滚，不能丢失活跃对象账本。
#[ktest]
fn managed_deallocate_failure_keeps_registry_record() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    let layout = Layout::from_size_align(64, 8).expect("valid managed layout");
    let handle = KERNEL_ALLOCATOR
        .allocate_managed_handle(layout, ManagedAllocFlags::default())
        .expect("allocate managed handle");
    let ptr = KERNEL_ALLOCATOR
        .managed_allocator()
        .resolve_handle(&handle)
        .expect("resolve managed handle");

    assert_eq!(
        KERNEL_ALLOCATOR.deallocate(ptr),
        Err(DeallocationError::ObjectStillReferenced)
    );
    assert!(KERNEL_ALLOCATOR.owns_allocation(ptr));

    let blocked = KERNEL_ALLOCATOR.audit();
    assert!(blocked.is_consistent());
    assert_eq!(
        blocked.managed_active_objects,
        before.managed_active_objects + 1
    );
    assert_eq!(
        blocked.managed_live_records,
        before.managed_live_records + 1
    );

    KERNEL_ALLOCATOR.managed_allocator().release_handle(handle);
    KERNEL_ALLOCATOR
        .deallocate(ptr)
        .expect("deallocate managed object after releasing handle");

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.managed_active_objects, before.managed_active_objects);
    assert_eq!(after.managed_live_records, before.managed_live_records);
}

/// 公开物理页 API 也必须进入 registry，这样 DMA/页表等外部调用者不会绕过账本审计。
#[ktest]
fn physical_api_updates_registry_audit() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    let allocation = KERNEL_ALLOCATOR
        .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
        .expect("allocate tracked physical page");
    let during = KERNEL_ALLOCATOR.audit();
    assert!(during.is_consistent());
    assert_eq!(
        during.registry_live_records,
        before.registry_live_records + 1
    );
    assert_eq!(
        during.registry_physical_records,
        before.registry_physical_records + 1
    );
    assert_eq!(during.registry_boot_records, before.registry_boot_records);
    assert!(KERNEL_ALLOCATOR.owns_allocation(allocation.paddr));

    KERNEL_ALLOCATOR
        .try_free_physical(allocation)
        .expect("free tracked physical page");
    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(
        after.registry_physical_records,
        before.registry_physical_records
    );
    assert!(!KERNEL_ALLOCATOR.owns_allocation(allocation.paddr));
}

/// 只保存 paddr 的外部子系统应通过 allocator 反查 registry 释放物理页。
#[ktest]
fn physical_api_can_free_by_tracked_address() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    let allocation = KERNEL_ALLOCATOR
        .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
        .expect("allocate tracked physical page");
    assert!(KERNEL_ALLOCATOR.owns_allocation(allocation.paddr));
    assert_eq!(
        KERNEL_ALLOCATOR
            .query_physical_allocation(allocation.paddr)
            .expect("query tracked physical page by paddr"),
        allocation
    );

    KERNEL_ALLOCATOR
        .try_free_physical_addr(allocation.paddr)
        .expect("free tracked physical page by paddr");
    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(
        after.registry_physical_records,
        before.registry_physical_records
    );
    assert!(!KERNEL_ALLOCATOR.owns_allocation(allocation.paddr));

    assert_eq!(
        KERNEL_ALLOCATOR.try_free_physical_addr(allocation.paddr),
        Err(PhysicalFreeError::UnknownPointer)
    );
    assert_eq!(
        KERNEL_ALLOCATOR.query_physical_allocation(allocation.paddr),
        Err(PhysicalFreeError::UnknownPointer)
    );
}

/// 物理页释放失败必须返回类型化原因，并且不能破坏 registry 中的活跃记录。
#[ktest]
fn physical_api_reports_typed_free_errors_and_preserves_record() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    let allocation = KERNEL_ALLOCATOR
        .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
        .expect("allocate tracked physical page");

    let wrong_order = PhysicalAllocation {
        order: allocation.order + 1,
        ..allocation
    };
    assert_eq!(
        KERNEL_ALLOCATOR.try_free_physical(wrong_order),
        Err(PhysicalFreeError::OrderMismatch {
            expected: allocation.order,
            actual: allocation.order + 1,
        })
    );
    assert!(KERNEL_ALLOCATOR.owns_allocation(allocation.paddr));
    let after_failed_free = KERNEL_ALLOCATOR.audit();
    assert!(after_failed_free.is_consistent());
    assert_eq!(
        after_failed_free.registry_live_records,
        before.registry_live_records + 1
    );
    assert_eq!(
        after_failed_free.registry_physical_records,
        before.registry_physical_records + 1
    );

    let wrong_page_size = PhysicalAllocation {
        page_size: allocation.page_size * 2,
        ..allocation
    };
    assert_eq!(
        KERNEL_ALLOCATOR.try_free_physical(wrong_page_size),
        Err(PhysicalFreeError::PageSizeMismatch {
            expected: allocation.page_size,
            actual: allocation.page_size * 2,
        })
    );
    assert!(KERNEL_ALLOCATOR.owns_allocation(allocation.paddr));

    let unknown = PhysicalAllocation {
        paddr: usize::MAX & !(PAGE_SIZE - 1),
        size: PAGE_SIZE,
        order: 0,
        page_size: PAGE_SIZE,
    };
    assert_eq!(
        KERNEL_ALLOCATOR.try_free_physical(unknown),
        Err(PhysicalFreeError::UnknownPointer)
    );

    KERNEL_ALLOCATOR
        .try_free_physical(allocation)
        .expect("free tracked physical page after failed attempts");
    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(
        after.registry_physical_records,
        before.registry_physical_records
    );
}

/// 物理页请求的 size/order/placement 校验必须在进入 buddy 修改状态前完成。
#[ktest]
fn physical_api_rejects_invalid_request_before_registry_update() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    assert!(
        KERNEL_ALLOCATOR
            .allocate_physical(PhysicalAllocRequest::new(0, PAGE_SIZE))
            .is_err()
    );
    assert!(
        KERNEL_ALLOCATOR
            .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, 3))
            .is_err()
    );
    assert!(
        KERNEL_ALLOCATOR
            .allocate_physical(PhysicalAllocRequest::new(usize::MAX, PAGE_SIZE))
            .is_err()
    );
    assert!(
        KERNEL_ALLOCATOR
            .allocate_physical(
                PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE)
                    .with_placement(MemoryPlacement::ExactPhys(PAGE_SIZE / 2)),
            )
            .is_err()
    );

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(
        after.registry_physical_records,
        before.registry_physical_records
    );
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
