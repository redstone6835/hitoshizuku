//! KERNEL_ALLOCATOR 集成测试。
//!
//! 在内核 allocator 初始化完成后执行，验证 allocate/deallocate 往返。

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};

use crate::boot::BootAllocator;
use crate::buddy::{DEFERRED_ORDER0_COALESCE_TARGET, DEFERRED_ORDER0_MIN_FREE_PERCENT};
use crate::error::DeallocationError;
use crate::gc::GcObjectHeader;
use crate::registry::AllocationRegistry;
use crate::request::{
    AllocationKind, AllocationRecord, MemoryDomain, MemoryPlacement, MemoryRequest, PagePolicy,
    PhysicalAllocRequest, Zeroing,
};
use crate::{
    ALLOCATOR_API_VERSION, AllocationError, AllocationRegistryAuditFlags, AllocatorAuditFlags,
    AllocatorAuditScope, AllocatorCapabilityFlags, AllocatorReclaimRequest, BuddyAuditFlags, GcRef,
    GcRefSlot, GcWeakRef, GcWeakRefSlot, KERNEL_ALLOCATOR, KernelHeapAuditFlags, ManagedAllocFlags,
    ManagedAuditFlags, PAGE_SIZE, PhysicalAllocation, PhysicalFreeError, SlabAuditFlags,
    TraceDescriptor,
};
use ktest::ktest;

#[allow(dead_code)]
#[repr(C)]
struct ManagedAuditLeaf {
    word: usize,
}

#[allow(dead_code)]
#[repr(C)]
struct ManagedAuditNode {
    strong: GcRef<ManagedAuditLeaf>,
    weak: GcWeakRef<ManagedAuditLeaf>,
}

const MANAGED_AUDIT_STRONG_OFFSET: usize = 0;
const MANAGED_AUDIT_WEAK_OFFSET: usize = core::mem::size_of::<GcRef<ManagedAuditLeaf>>();
static MANAGED_AUDIT_STRONG_OFFSETS: [usize; 1] = [MANAGED_AUDIT_STRONG_OFFSET];
static MANAGED_AUDIT_WEAK_OFFSETS: [usize; 1] = [MANAGED_AUDIT_WEAK_OFFSET];
static MANAGED_AUDIT_NODE_DESCRIPTOR: TraceDescriptor = TraceDescriptor::exact_layout(
    core::mem::size_of::<ManagedAuditNode>(),
    core::mem::align_of::<ManagedAuditNode>(),
    &MANAGED_AUDIT_STRONG_OFFSETS,
)
.with_weak_references(&MANAGED_AUDIT_WEAK_OFFSETS);

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
    let after_alloc = KERNEL_ALLOCATOR.layer_stats().kheap;
    KERNEL_ALLOCATOR
        .deallocate(first.ptr)
        .expect("cache first kheap range");

    let after_free = KERNEL_ALLOCATOR.layer_stats().kheap;
    assert!(after_free.cache_inserts >= kheap_before.cache_inserts + 1);
    if after_alloc.cache_hits > kheap_before.cache_hits {
        // 前序测试可能已经预热了 kheap cache。此时本轮分配先消耗一个缓存页，
        // 释放后只需要回到测试前缓存水位，而不是继续增加 cached_pages。
        assert!(after_free.cached_pages >= kheap_before.cached_pages);
    } else {
        assert!(after_free.cached_pages >= kheap_before.cached_pages + 1);
    }

    let cached = KERNEL_ALLOCATOR.audit();
    assert!(cached.is_consistent());
    assert_eq!(cached.registry_live_records, before.registry_live_records);
    assert_eq!(cached.kheap_active_allocs, before.kheap_active_allocs);
    let kheap_audit = KERNEL_ALLOCATOR.kheap_audit();
    assert!(kheap_audit.is_consistent());
    assert_eq!(kheap_audit.flags, KernelHeapAuditFlags::empty());
    assert_eq!(kheap_audit.scanned_cached_pages, after_free.cached_pages);
    assert_eq!(kheap_audit.scanned_cached_ranges, after_free.cached_ranges);

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

/// kheap cache 满桶时应保留最新释放的 range，而不是把最热对象直接释放回后端。
#[ktest]
fn kheap_full_cache_keeps_latest_freed_range_hot() {
    const COUNT: usize = 160;
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    let kheap_before = KERNEL_ALLOCATOR.layer_stats().kheap;
    let mut records = [None; COUNT];

    for slot in &mut records {
        let record = KERNEL_ALLOCATOR
            .allocate(MemoryRequest::new(
                MemoryDomain::Kernel,
                PAGE_SIZE,
                PAGE_SIZE,
            ))
            .expect("allocate kheap cache fill object");
        assert_eq!(record.kind, AllocationKind::Large);
        *slot = Some(record);
    }

    let sentinel = records[COUNT - 1].take().expect("sentinel record exists");
    for slot in records.iter_mut().take(COUNT - 1) {
        let record = slot.take().expect("record exists");
        KERNEL_ALLOCATOR
            .deallocate(record.ptr)
            .expect("deallocate cache fill object");
    }

    KERNEL_ALLOCATOR
        .deallocate(sentinel.ptr)
        .expect("deallocate sentinel into full cache");
    let after_full_free = KERNEL_ALLOCATOR.layer_stats().kheap;
    assert!(after_full_free.cache_full_releases > kheap_before.cache_full_releases);

    let reused = KERNEL_ALLOCATOR
        .allocate(MemoryRequest::new(
            MemoryDomain::Kernel,
            PAGE_SIZE,
            PAGE_SIZE,
        ))
        .expect("allocate from full kheap cache");
    assert_eq!(reused.ptr, sentinel.ptr);
    assert!(KERNEL_ALLOCATOR.layer_stats().kheap.cache_hits > after_full_free.cache_hits);

    KERNEL_ALLOCATOR
        .deallocate(reused.ptr)
        .expect("deallocate reused sentinel");
    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.kheap_active_allocs, before.kheap_active_allocs);
}

/// 外部维护 API 应能主动释放 kheap 缓存页，并保持 registry 账本不变。
#[ktest]
fn allocator_reclaim_releases_kheap_cached_ranges() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    let record = KERNEL_ALLOCATOR
        .allocate(MemoryRequest::new(
            MemoryDomain::Kernel,
            PAGE_SIZE,
            PAGE_SIZE,
        ))
        .expect("allocate cacheable kheap object");
    assert_eq!(record.kind, AllocationKind::Large);
    KERNEL_ALLOCATOR
        .deallocate(record.ptr)
        .expect("cache kheap object");

    let cached = KERNEL_ALLOCATOR.layer_stats().kheap;
    assert!(cached.cached_ranges > 0);

    let reclaim = KERNEL_ALLOCATOR
        .reclaim(
            AllocatorReclaimRequest::caches()
                .without_slab_cache_flush()
                .without_slab_empty_reclaim()
                .without_physical_deferred_reclaim(),
        )
        .expect("reclaim kheap cache");
    assert!(reclaim.kheap.released_ranges > 0);
    assert!(reclaim.kheap.released_pages > 0);
    assert!(reclaim.reclaimed_bytes() >= PAGE_SIZE);

    let kheap_after = KERNEL_ALLOCATOR.layer_stats().kheap;
    assert_eq!(kheap_after.cached_ranges, 0);

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
    // `format!` 会经全局 allocator 创建临时 String；审计前必须释放它，
    // 否则本测试自身会让 registry live 计数比基线多一个对象。
    drop(debug);

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

/// slab cache 满桶时必须先 flush 旧 cached 对象，再把当前释放对象放回 cache。
#[ktest]
fn slab_cache_overflow_free_keeps_accounting_consistent() {
    const COUNT: usize = 96;
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    let slab_before = KERNEL_ALLOCATOR.layer_stats().slab;
    let mut records = [None; COUNT];

    for slot in &mut records {
        let record = KERNEL_ALLOCATOR
            .allocate(MemoryRequest::new(MemoryDomain::Kernel, 64, 8))
            .expect("allocate cache overflow object");
        assert_eq!(record.kind, AllocationKind::Small);
        *slot = Some(record);
    }

    let during = KERNEL_ALLOCATOR.audit();
    assert!(during.is_consistent());
    assert_eq!(
        during.registry_live_records,
        before.registry_live_records + COUNT
    );
    assert_eq!(during.slab_live_records, before.slab_live_records + COUNT);

    for slot in records.iter_mut().rev() {
        let record = slot.take().expect("record exists");
        KERNEL_ALLOCATOR
            .deallocate(record.ptr)
            .expect("deallocate cache overflow object");
    }

    let slab_after = KERNEL_ALLOCATOR.layer_stats().slab;
    assert!(slab_after.cache_flushes > slab_before.cache_flushes);
    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.slab_live_records, before.slab_live_records);
    assert_eq!(after.slab_active_objects, before.slab_active_objects);
}

/// 外部维护 API 应能冲刷 slab per-CPU cache，而不是只能等 cache 满桶时被动 flush。
#[ktest]
fn allocator_reclaim_flushes_slab_cpu_caches() {
    const COUNT: usize = 16;
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    let mut records = [None; COUNT];

    for slot in &mut records {
        let record = KERNEL_ALLOCATOR
            .allocate(MemoryRequest::new(MemoryDomain::Kernel, 64, 8))
            .expect("allocate slab cache object");
        assert_eq!(record.kind, AllocationKind::Small);
        *slot = Some(record);
    }
    for slot in &mut records {
        let record = slot.take().expect("record exists");
        KERNEL_ALLOCATOR
            .deallocate(record.ptr)
            .expect("stage slab object into cache");
    }

    let reclaim = KERNEL_ALLOCATOR
        .reclaim(
            AllocatorReclaimRequest::caches()
                .with_kheap_cached_ranges(0)
                .without_slab_empty_reclaim()
                .without_physical_deferred_reclaim(),
        )
        .expect("flush slab cpu caches");
    assert!(reclaim.slab.flushed_cached_objects > 0);
    assert_eq!(reclaim.kheap.released_ranges, 0);

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.slab_active_objects, before.slab_active_objects);
}

/// 大规模 small object churn 不能在释放后继续单调吃页，更不能退化成 kheap 整页分配。
#[ktest]
fn allocator_reclaim_after_slab_churn_stops_page_growth() {
    const COUNT: usize = 384;

    KERNEL_ALLOCATOR
        .reclaim(AllocatorReclaimRequest::caches())
        .expect("quiesce allocator caches before churn");
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    let baseline = KERNEL_ALLOCATOR.layer_stats();

    run_small_churn_batch::<COUNT>();
    let first_reclaim = KERNEL_ALLOCATOR
        .reclaim(AllocatorReclaimRequest::caches())
        .expect("reclaim after first slab churn");
    assert!(first_reclaim.slab.flushed_cached_objects > 0);
    assert!(first_reclaim.slab.reclaimed_slabs > 0);

    let after_first = KERNEL_ALLOCATOR.audit();
    assert!(after_first.is_consistent());
    assert_eq!(
        after_first.registry_live_records,
        before.registry_live_records
    );
    assert_eq!(after_first.slab_live_records, before.slab_live_records);
    assert_eq!(after_first.slab_active_objects, before.slab_active_objects);
    let first_layers = KERNEL_ALLOCATOR.layer_stats();
    assert_eq!(
        first_layers.kheap.alloc_requests,
        baseline.kheap.alloc_requests
    );
    assert_eq!(
        first_layers.kheap.active_allocs,
        baseline.kheap.active_allocs
    );

    run_small_churn_batch::<COUNT>();
    let second_reclaim = KERNEL_ALLOCATOR
        .reclaim(AllocatorReclaimRequest::caches())
        .expect("reclaim after second slab churn");
    assert!(second_reclaim.slab.flushed_cached_objects > 0);

    let after_second = KERNEL_ALLOCATOR.audit();
    assert!(after_second.is_consistent());
    assert_eq!(
        after_second.registry_live_records,
        before.registry_live_records
    );
    assert_eq!(after_second.slab_live_records, before.slab_live_records);
    assert_eq!(after_second.slab_active_objects, before.slab_active_objects);
    let second_layers = KERNEL_ALLOCATOR.layer_stats();
    assert_eq!(
        second_layers.kheap.alloc_requests,
        baseline.kheap.alloc_requests
    );
    assert_eq!(
        second_layers.kheap.active_allocs,
        baseline.kheap.active_allocs
    );
    assert!(
        second_layers.slab.active_pages <= first_layers.slab.active_pages,
        "slab pages kept growing after full reclaim: first={} second={}",
        first_layers.slab.active_pages,
        second_layers.slab.active_pages
    );
}

fn run_small_churn_batch<const COUNT: usize>() {
    let mut records = [None; COUNT];
    for slot in &mut records {
        let record = KERNEL_ALLOCATOR
            .allocate(MemoryRequest::new(MemoryDomain::Kernel, 64, 8))
            .expect("allocate slab churn object");
        assert_eq!(record.kind, AllocationKind::Small);
        *slot = Some(record);
    }
    for slot in records.iter_mut().rev() {
        let record = slot.take().expect("record exists");
        KERNEL_ALLOCATOR
            .deallocate(record.ptr)
            .expect("deallocate slab churn object");
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

/// typed reallocate 原地扩容时仍必须遵守 Zeroed 请求，清零新增逻辑区间。
#[ktest]
fn reallocate_small_in_place_zeroed_growth_clears_new_bytes() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    let record = KERNEL_ALLOCATOR
        .allocate(MemoryRequest::new(MemoryDomain::Kernel, 48, 8))
        .expect("allocate object with slab headroom");
    let grown_size = record.usable_size;
    assert!(grown_size > record.size);
    unsafe {
        core::ptr::write_bytes(record.ptr as *mut u8, 0x5A, record.size);
        // allocator 内部测试可以污染 backend 已保留但尚未暴露给调用者的可用区间，
        // 用来证明原地扩容时的 Zeroed 语义确实覆盖新增逻辑字节。
        core::ptr::write_bytes(
            (record.ptr + record.size) as *mut u8,
            0xA5,
            grown_size - record.size,
        );
    }

    let resized = KERNEL_ALLOCATOR
        .reallocate(
            record.ptr,
            MemoryRequest::new(MemoryDomain::Kernel, grown_size, 8).with_zeroing(Zeroing::Zeroed),
        )
        .expect("reallocate in place with zeroed growth");
    assert_eq!(resized.ptr, record.ptr);
    assert_eq!(resized.size, grown_size);

    let bytes = unsafe { core::slice::from_raw_parts(resized.ptr as *const u8, grown_size) };
    assert!(bytes[..record.size].iter().all(|&byte| byte == 0x5A));
    assert!(bytes[record.size..grown_size].iter().all(|&byte| byte == 0));

    KERNEL_ALLOCATOR
        .deallocate(resized.ptr)
        .expect("deallocate zero-grown object");
    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.slab_live_records, before.slab_live_records);
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
    assert_eq!(stats.chain_corruptions, 0);
    assert!(stats.max_shard_live_records > 0);
    assert!(stats.max_shard_live_records <= stats.live_records);
    assert!(stats.nodes_allocated >= stats.live_records);
    assert!(stats.node_refills > 0);

    let audit = KERNEL_ALLOCATOR.registry_audit();
    assert_eq!(audit.flags, AllocationRegistryAuditFlags::empty());
    assert_eq!(audit.scanned_live_records, stats.live_records);
    assert_eq!(audit.scanned_live_small, stats.live_small);
    assert_eq!(audit.scanned_max_chain_len, stats.max_chain_len);

    let snapshot = KERNEL_ALLOCATOR.registry_snapshot();
    assert_eq!(snapshot.audit.flags, AllocationRegistryAuditFlags::empty());
    assert_eq!(
        snapshot.audit.scanned_live_records,
        snapshot.stats.live_records
    );
    assert_eq!(snapshot.audit.scanned_live_small, snapshot.stats.live_small);
    assert_eq!(snapshot.audit.scanned_free_nodes, snapshot.stats.free_nodes);

    for slot in records.iter_mut().rev() {
        let record = slot.take().expect("record exists");
        KERNEL_ALLOCATOR
            .deallocate(record.ptr)
            .expect("deallocate tracked object");
    }

    let stats = KERNEL_ALLOCATOR.registry_stats();
    assert!(stats.free_nodes > 0);
    assert!(stats.nodes_allocated >= stats.free_nodes);

    let audit = KERNEL_ALLOCATOR.registry_audit();
    assert_eq!(audit.flags, AllocationRegistryAuditFlags::empty());
    assert_eq!(audit.scanned_live_records, stats.live_records);
    assert_eq!(audit.scanned_free_nodes, stats.free_nodes);
    assert_eq!(audit.scanned_max_chain_len, stats.max_chain_len);

    let snapshot = KERNEL_ALLOCATOR.registry_snapshot();
    assert_eq!(snapshot.audit.flags, AllocationRegistryAuditFlags::empty());
    assert_eq!(
        snapshot.audit.scanned_live_records,
        snapshot.stats.live_records
    );
    assert_eq!(snapshot.audit.scanned_free_nodes, snapshot.stats.free_nodes);
    assert_eq!(
        snapshot.audit.scanned_max_chain_len,
        snapshot.stats.max_chain_len
    );
}

/// registry 删除热点链表节点后，O(1) 统计中的最大链长必须随当前结构回落。
#[ktest]
fn registry_max_chain_len_shrinks_after_collision_removal() {
    const COUNT: usize = 8;
    let registry = AllocationRegistry::new();
    let boot = BootAllocator::new();
    assert!(registry.init_with_buckets(&boot, 1));

    let stats = registry.stats();
    assert_eq!(stats.bucket_count, stats.shard_count);
    let shard_mask = stats.shard_count - 1;
    let mut ptrs = [0usize; COUNT];
    let mut filled = 0usize;
    let mut candidate = 0x1000usize;
    while filled < COUNT {
        if registry_test_shard(candidate, shard_mask) == 0 {
            ptrs[filled] = candidate;
            filled += 1;
        }
        candidate = candidate.saturating_add(8);
        assert!(candidate < 0x10_0000);
    }

    for &ptr in &ptrs {
        let record = AllocationRecord::new(AllocationKind::Small, MemoryDomain::Kernel, ptr)
            .with_sizes(32, 32, 8);
        registry
            .register_result(&boot, record)
            .expect("register colliding record");
    }

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.audit.flags, AllocationRegistryAuditFlags::empty());
    assert_eq!(snapshot.stats.live_records, COUNT);
    assert_eq!(snapshot.stats.max_chain_len, COUNT);
    assert_eq!(snapshot.audit.scanned_max_chain_len, COUNT);

    for (idx, &ptr) in ptrs.iter().enumerate() {
        registry
            .remove_result(ptr)
            .expect("remove colliding record");
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.audit.flags, AllocationRegistryAuditFlags::empty());
        assert_eq!(snapshot.stats.live_records, COUNT - idx - 1);
        assert_eq!(
            snapshot.stats.max_chain_len,
            snapshot.audit.scanned_max_chain_len
        );
    }

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.stats.live_records, 0);
    assert_eq!(snapshot.stats.max_chain_len, 0);
    assert_eq!(snapshot.audit.scanned_max_chain_len, 0);
}

fn registry_test_shard(ptr: usize, shard_mask: usize) -> usize {
    ((ptr >> 3) ^ (ptr >> 11) ^ (ptr >> 19) ^ (ptr >> 27)) & shard_mask
}

/// 审计接口应能把 registry 和后端计数稳定对齐。
#[ktest]
fn allocator_audit_reports_consistent_layer_accounting() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    assert_eq!(before.flags, AllocatorAuditFlags::empty());
    assert!(before.registry_structure_scanned);
    assert_eq!(
        before.registry_structure.flags,
        AllocationRegistryAuditFlags::empty()
    );
    assert!(before.phys_structure_scanned);
    assert_eq!(before.phys_structure.flags, BuddyAuditFlags::empty());
    assert_eq!(
        before.phys_structure.scanned_total_pages,
        KERNEL_ALLOCATOR.buddy_stats().total_pages
    );
    assert!(before.slab_structure_scanned);
    assert_eq!(before.slab_structure.flags, SlabAuditFlags::empty());
    assert!(before.kheap_structure_scanned);
    assert_eq!(before.kheap_structure.flags, KernelHeapAuditFlags::empty());
    assert!(before.managed_structure_scanned);
    assert_eq!(before.managed_structure.flags, ManagedAuditFlags::empty());
    assert_eq!(
        before.slab_structure.scanned_active_objects,
        before.slab_active_objects
    );
    assert_eq!(
        before.slab_structure.scanned_active_bytes,
        before.slab_active_bytes
    );

    let small = KERNEL_ALLOCATOR
        .allocate(MemoryRequest::new(MemoryDomain::Kernel, 32, 8))
        .expect("allocate audited small object");
    let large = KERNEL_ALLOCATOR
        .allocate(MemoryRequest::new(MemoryDomain::Kernel, 4096, 8))
        .expect("allocate audited large object");

    let during = KERNEL_ALLOCATOR.audit();
    assert!(during.is_consistent());
    assert!(during.registry_structure_scanned);
    assert!(during.phys_structure_scanned);
    assert!(during.slab_structure_scanned);
    assert!(during.kheap_structure_scanned);
    assert!(during.managed_structure_scanned);
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
    assert_eq!(
        during.kheap_structure.scanned_active_allocs,
        during.kheap_active_allocs
    );
    assert_eq!(
        during.kheap_structure.scanned_active_pages * PAGE_SIZE,
        during.kheap_active_bytes
    );
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
    assert_eq!(
        during.slab_structure.scanned_active_objects,
        during.slab_active_objects
    );
    assert_eq!(
        during.slab_structure.scanned_active_bytes,
        during.slab_active_bytes
    );
    assert_eq!(
        during.slab_structure.scanned_active_pages * PAGE_SIZE,
        during.slab_backing_bytes
    );
    assert_eq!(during.phys_structure.flags, BuddyAuditFlags::empty());
    assert_eq!(
        during
            .phys_structure
            .scanned_allocated_pages
            .saturating_add(during.phys_structure.scanned_free_pages)
            .saturating_add(during.phys_structure.scanned_reserved_pages),
        during.phys_structure.scanned_total_pages
    );
    assert_eq!(during.slab_structure.flags, SlabAuditFlags::empty());
    assert_eq!(during.kheap_structure.flags, KernelHeapAuditFlags::empty());
    assert_eq!(during.managed_structure.flags, ManagedAuditFlags::empty());
    assert_eq!(during.kheap_active_bytes, during.kheap_page_bytes);
    assert_eq!(
        during.managed_structure.scanned_active_objects,
        during.managed_active_objects
    );
    assert_eq!(
        during.managed_structure.scanned_active_bytes,
        KERNEL_ALLOCATOR.managed_stats().active_bytes
    );

    KERNEL_ALLOCATOR
        .deallocate(small.ptr)
        .expect("deallocate audited small object");
    KERNEL_ALLOCATOR
        .deallocate(large.ptr)
        .expect("deallocate audited large object");

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert!(after.registry_structure_scanned);
    assert!(after.phys_structure_scanned);
    assert!(after.slab_structure_scanned);
    assert!(after.kheap_structure_scanned);
    assert!(after.managed_structure_scanned);
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

/// counters-only 审计只检查 O(1) 分层账本，不扫描 registry 链表。
#[ktest]
fn allocator_counter_audit_skips_registry_scan() {
    let full = KERNEL_ALLOCATOR.audit();
    assert!(full.is_consistent());
    assert!(full.registry_structure_scanned);
    assert!(full.phys_structure_scanned);
    assert!(full.slab_structure_scanned);
    assert!(full.kheap_structure_scanned);
    assert!(full.managed_structure_scanned);

    let counters = KERNEL_ALLOCATOR.audit_counters();
    assert!(counters.is_consistent());
    assert_eq!(counters.flags, AllocatorAuditFlags::empty());
    assert!(!counters.registry_structure_scanned);
    assert_eq!(counters.registry_structure.flags.bits(), 0);
    assert_eq!(counters.registry_structure.scanned_live_records, 0);
    assert!(!counters.phys_structure_scanned);
    assert_eq!(counters.phys_structure.flags.bits(), 0);
    assert_eq!(counters.phys_structure.scanned_total_pages, 0);
    assert!(!counters.slab_structure_scanned);
    assert_eq!(counters.slab_structure.flags.bits(), 0);
    assert_eq!(counters.slab_structure.scanned_active_objects, 0);
    assert!(!counters.kheap_structure_scanned);
    assert_eq!(counters.kheap_structure.flags.bits(), 0);
    assert_eq!(counters.kheap_structure.scanned_cached_ranges, 0);
    assert!(!counters.managed_structure_scanned);
    assert_eq!(counters.managed_structure.flags.bits(), 0);
    assert_eq!(counters.managed_structure.scanned_active_objects, 0);
    assert_eq!(counters.registry_live_records, full.registry_live_records);
    assert_eq!(
        counters.registry_nodes_accounted,
        counters.registry_node_capacity
    );

    let scoped = KERNEL_ALLOCATOR.audit_with_scope(AllocatorAuditScope::CountersOnly);
    assert_eq!(scoped, counters);
    let scoped_full = KERNEL_ALLOCATOR.audit_with_scope(AllocatorAuditScope::FullRegistry);
    assert!(scoped_full.registry_structure_scanned);
    assert!(scoped_full.phys_structure_scanned);
    assert!(scoped_full.slab_structure_scanned);
    assert!(scoped_full.kheap_structure_scanned);
    assert!(scoped_full.managed_structure_scanned);
    assert_eq!(
        scoped_full.registry_live_records,
        full.registry_live_records
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
    assert!(text.contains("mode=full"));
    assert!(text.contains("reg_struct=0"));
    assert!(text.contains("phys_struct=0"));
    assert!(text.contains("slab_struct=0"));
    assert!(text.contains("kheap_struct=0"));
    assert!(text.contains("managed_struct=0"));
    assert!(text.contains("managed_refs=0/0/0"));
    assert!(text.contains("Hot: slab_hit="));
    assert!(text.contains("phys_split="));
    assert!(text.contains("phys_defer="));
    assert!(text.contains("phys_reclaim="));
    assert!(text.contains("phys_corrupt=0"));
    assert!(text.contains("reg_corrupt=0"));
    assert!(text.contains("boot="));
    assert!(text.contains("physrec="));

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.registry_node_capacity, before.registry_node_capacity);
}

/// 轻量诊断应避免 registry 全量扫描，并在文本中明确标记采样范围。
#[ktest]
fn allocator_counter_diagnostic_reports_skipped_scan() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    let mut buf = [0u8; 2048];
    let len = KERNEL_ALLOCATOR.format_diagnostic_counters(&mut buf);
    assert!(len > 0);
    let text = core::str::from_utf8(&buf[..len]).expect("diagnostic is ascii");
    assert!(text.contains("Audit: ok=1"));
    assert!(text.contains("mode=counters"));
    assert!(text.contains("reg_struct=skip"));
    assert!(text.contains("phys_struct=skip"));
    assert!(text.contains("slab_struct=skip"));
    assert!(text.contains("kheap_struct=skip"));
    assert!(text.contains("managed_struct=skip"));
    assert!(text.contains("managed_refs=skip"));
    assert!(text.contains("scan=skip"));
    assert!(text.contains("Hot: slab_hit="));
    assert!(text.contains("phys_meta="));

    let scoped_len =
        KERNEL_ALLOCATOR.format_diagnostic_with_scope(&mut buf, AllocatorAuditScope::CountersOnly);
    assert!(scoped_len > 0);
    let scoped = core::str::from_utf8(&buf[..scoped_len]).expect("diagnostic is ascii");
    assert!(scoped.contains("mode=counters"));

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
    let registry = KERNEL_ALLOCATOR.registry_stats();
    let expected_registry_load = if registry.bucket_count == 0 {
        0
    } else {
        ((registry.live_records as u128 * 1000) / registry.bucket_count as u128)
            .min(u32::MAX as u128) as u32
    };
    assert!(hot.phys_alloc_failure_per_mille <= 1000);
    assert!(hot.phys_defer_per_free_mille <= 1000);
    assert!(hot.phys_metadata_load_per_mille <= 1000);
    assert_eq!(hot.phys_chain_corruptions, 0);
    assert!(hot.slab_cache_hit_per_mille <= 1000);
    assert!(hot.slab_cache_miss_per_mille <= 1000);
    assert_eq!(
        hot.registry_live_per_bucket_per_mille,
        expected_registry_load
    );
    assert_eq!(hot.registry_chain_corruptions, 0);
    assert!(hot.kheap_failure_per_mille <= 1000);
    assert!(hot.kheap_realloc_per_mille <= 1000);
    assert!(hot.kernel_vmem_largest_free_percent <= 100);
    assert!(hot.managed_fragmentation_per_mille <= 1000);

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.registry_node_capacity, before.registry_node_capacity);
}

/// capability 快照给外部模块提供稳定的 allocator 能力发现入口。
#[ktest]
fn allocator_capabilities_report_stable_external_api() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());

    let caps = KERNEL_ALLOCATOR.capabilities();
    assert_eq!(caps.api_version, ALLOCATOR_API_VERSION);
    assert_eq!(caps.page_size, PAGE_SIZE);
    assert_eq!(caps.max_small_size, crate::MAX_SMALL_SIZE);
    assert_eq!(caps.max_cpus, crate::MAX_CPUS);
    assert!(caps.supports(AllocatorCapabilityFlags::TYPED_MEMORY_REQUEST));
    assert!(caps.supports(AllocatorCapabilityFlags::TRACKED_PHYSICAL_API));
    assert!(caps.supports(AllocatorCapabilityFlags::REGISTRY_SNAPSHOT));
    assert!(caps.supports(AllocatorCapabilityFlags::COUNTERS_AUDIT));
    assert!(caps.supports(AllocatorCapabilityFlags::FULL_STRUCTURE_AUDIT));
    assert!(caps.supports(AllocatorCapabilityFlags::BUDDY_STRUCTURE_AUDIT));
    assert!(caps.supports(AllocatorCapabilityFlags::SLAB_STRUCTURE_AUDIT));
    assert!(caps.supports(AllocatorCapabilityFlags::KHEAP_STRUCTURE_AUDIT));
    assert!(caps.supports(AllocatorCapabilityFlags::MANAGED_STRUCTURE_AUDIT));
    assert!(caps.supports(AllocatorCapabilityFlags::MANAGED_REFERENCE_AUDIT));
    assert!(caps.supports(AllocatorCapabilityFlags::CACHE_RECLAIM));
    assert!(caps.supports(AllocatorCapabilityFlags::HOTSPOT_SUMMARY));
    assert!(caps.supports(AllocatorCapabilityFlags::MANAGED_GC));
    assert!(caps.supports(AllocatorCapabilityFlags::RELOCATION_OBSERVER));
    assert!(caps.supports(AllocatorCapabilityFlags::EXACT_ROOT_PROVIDER));
    assert!(caps.supports(AllocatorCapabilityFlags::GC_CRITICAL_SECTION_HOOKS));
    assert_eq!(
        caps.managed_enabled,
        KERNEL_ALLOCATOR.managed_stats().enabled
    );

    let counter = KERNEL_ALLOCATOR.audit_counters();
    assert!(counter.is_consistent());
    assert!(!counter.registry_structure_scanned);
    let full = KERNEL_ALLOCATOR.audit();
    assert!(full.is_consistent());
    assert!(full.managed_structure_scanned);

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
    let managed_audit = KERNEL_ALLOCATOR.managed_audit();
    assert!(managed_audit.is_consistent());
    assert_eq!(managed_audit.flags, ManagedAuditFlags::empty());
    assert_eq!(
        managed_audit.scanned_active_objects,
        blocked.managed_active_objects
    );
    assert_eq!(
        managed_audit.scanned_active_bytes,
        KERNEL_ALLOCATOR.managed_stats().active_bytes
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

/// managed audit 应扫描精确 trace descriptor 中声明的强/弱引用字段。
#[ktest]
fn managed_audit_scans_typed_reference_slots() {
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    let before_managed = KERNEL_ALLOCATOR.managed_audit();
    assert!(before_managed.is_consistent());

    let node = KERNEL_ALLOCATOR
        .allocate_managed_handle(
            Layout::new::<ManagedAuditNode>(),
            ManagedAllocFlags::new().with_trace_descriptor(&MANAGED_AUDIT_NODE_DESCRIPTOR),
        )
        .expect("allocate managed reference node");
    let strong_target = KERNEL_ALLOCATOR
        .allocate_managed_handle(
            Layout::new::<ManagedAuditLeaf>(),
            ManagedAllocFlags::default(),
        )
        .expect("allocate managed strong target");
    let weak_target = KERNEL_ALLOCATOR
        .allocate_managed_handle(
            Layout::new::<ManagedAuditLeaf>(),
            ManagedAllocFlags::default(),
        )
        .expect("allocate managed weak target");

    let node_ptr = KERNEL_ALLOCATOR
        .managed_allocator()
        .resolve_handle(&node)
        .expect("resolve managed node");
    let strong_ptr = KERNEL_ALLOCATOR
        .managed_allocator()
        .resolve_handle(&strong_target)
        .expect("resolve managed strong target");
    let weak_ptr = KERNEL_ALLOCATOR
        .managed_allocator()
        .resolve_handle(&weak_target)
        .expect("resolve managed weak target");

    KERNEL_ALLOCATOR
        .managed_allocator()
        .store_ref(
            &node,
            GcRefSlot::<ManagedAuditLeaf>::new(MANAGED_AUDIT_STRONG_OFFSET),
            GcRef::from_raw(strong_ptr),
        )
        .expect("store managed strong reference");
    KERNEL_ALLOCATOR
        .managed_allocator()
        .store_weak_ref(
            &node,
            GcWeakRefSlot::<ManagedAuditLeaf>::new(MANAGED_AUDIT_WEAK_OFFSET),
            GcWeakRef::from_raw(weak_ptr),
        )
        .expect("store managed weak reference");

    let audit = KERNEL_ALLOCATOR.managed_audit();
    assert!(audit.is_consistent());
    assert_eq!(audit.flags, ManagedAuditFlags::empty());
    assert_eq!(
        audit.scanned_strong_reference_slots,
        before_managed.scanned_strong_reference_slots + 1
    );
    assert_eq!(
        audit.scanned_weak_reference_slots,
        before_managed.scanned_weak_reference_slots + 1
    );
    assert_eq!(
        audit.scanned_stale_weak_reference_slots,
        before_managed.scanned_stale_weak_reference_slots
    );

    let header_addr = node_ptr - GcObjectHeader::HEADER_SIZE;
    let saved_header = unsafe { *(header_addr as *const GcObjectHeader) };
    let mut corrupted_header = saved_header;
    corrupted_header.trace_descriptor_ptr = 0;
    unsafe {
        *(header_addr as *mut GcObjectHeader) = corrupted_header;
    }
    let drift = KERNEL_ALLOCATOR.managed_audit();
    assert!(
        drift
            .flags
            .contains(ManagedAuditFlags::OBJECT_HEADER_MISMATCH)
    );
    assert_eq!(
        drift.scanned_strong_reference_slots,
        before_managed.scanned_strong_reference_slots + 1
    );
    unsafe {
        *(header_addr as *mut GcObjectHeader) = saved_header;
    }
    let restored = KERNEL_ALLOCATOR.managed_audit();
    assert!(restored.is_consistent());

    KERNEL_ALLOCATOR.managed_allocator().release_handle(node);
    KERNEL_ALLOCATOR
        .deallocate(node_ptr)
        .expect("deallocate managed reference node");
    KERNEL_ALLOCATOR
        .managed_allocator()
        .release_handle(strong_target);
    KERNEL_ALLOCATOR
        .deallocate(strong_ptr)
        .expect("deallocate managed strong target");
    KERNEL_ALLOCATOR
        .managed_allocator()
        .release_handle(weak_target);
    KERNEL_ALLOCATOR
        .deallocate(weak_ptr)
        .expect("deallocate managed weak target");

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(after.managed_active_objects, before.managed_active_objects);
    assert_eq!(after.managed_live_records, before.managed_live_records);
}

/// order-0 物理页短周期释放应优先进入受限延迟合并水位，减少下一轮分配的 split 抖动。
#[ktest]
fn physical_page_churn_uses_deferred_order0_coalesce() {
    const COUNT: usize = 32;
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    let buddy_before = KERNEL_ALLOCATOR.buddy_stats();
    let can_defer = buddy_before.total_pages != 0
        && (buddy_before.free_pages.saturating_mul(100)) / buddy_before.total_pages
            >= DEFERRED_ORDER0_MIN_FREE_PERCENT
        && buddy_before.free_count_per_order[0] < DEFERRED_ORDER0_COALESCE_TARGET;
    let mut pages = [None; COUNT];

    for slot in &mut pages {
        let page = KERNEL_ALLOCATOR
            .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
            .expect("allocate churn physical page");
        *slot = Some(page);
    }
    for slot in pages.iter_mut().rev() {
        let page = slot.take().expect("physical page exists");
        KERNEL_ALLOCATOR
            .try_free_physical(page)
            .expect("free churn physical page");
    }

    let buddy_after = KERNEL_ALLOCATOR.buddy_stats();
    assert!(buddy_after.free_requests >= buddy_before.free_requests + COUNT as u64);
    assert!(buddy_after.deferred_coalesce_count >= buddy_before.deferred_coalesce_count);
    if can_defer {
        assert!(buddy_after.deferred_coalesce_count > buddy_before.deferred_coalesce_count);
    }

    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(
        after.registry_physical_records,
        before.registry_physical_records
    );
}

/// ExactPhys 高阶分配应能先回收 deferred order-0 页，避免热页缓存造成假性碎片。
#[ktest]
fn exact_physical_alloc_reclaims_deferred_order0_pages() {
    const COUNT: usize = 16;
    let before = KERNEL_ALLOCATOR.audit();
    assert!(before.is_consistent());
    let mut pages = [None; COUNT];

    for slot in &mut pages {
        let page = KERNEL_ALLOCATOR
            .allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE))
            .expect("allocate exact-reclaim source page");
        *slot = Some(page);
    }

    let mut exact_base = None;
    for left in 0..COUNT {
        for right in (left + 1)..COUNT {
            let a = pages[left].expect("left page exists").paddr;
            let b = pages[right].expect("right page exists").paddr;
            let base = a.min(b);
            let next = a.max(b);
            if base.is_multiple_of(PAGE_SIZE * 2) && next == base + PAGE_SIZE {
                exact_base = Some(base);
                break;
            }
        }
        if exact_base.is_some() {
            break;
        }
    }
    let exact_base = exact_base.expect("allocated pages should contain an order-1 buddy pair");

    for slot in pages.iter_mut().rev() {
        let page = slot.take().expect("source page exists");
        KERNEL_ALLOCATOR
            .try_free_physical(page)
            .expect("free exact-reclaim source page");
    }
    let before_reclaim = KERNEL_ALLOCATOR.buddy_stats();

    let allocation = KERNEL_ALLOCATOR
        .allocate_physical(
            PhysicalAllocRequest::new(PAGE_SIZE * 2, PAGE_SIZE * 2)
                .with_placement(MemoryPlacement::ExactPhys(exact_base)),
        )
        .expect("allocate exact order-1 block from deferred pages");
    assert_eq!(allocation.paddr, exact_base);
    let after_reclaim = KERNEL_ALLOCATOR.buddy_stats();
    assert!(after_reclaim.deferred_reclaim_count > before_reclaim.deferred_reclaim_count);

    KERNEL_ALLOCATOR
        .try_free_physical(allocation)
        .expect("free exact-reclaimed block");
    let after = KERNEL_ALLOCATOR.audit();
    assert!(after.is_consistent());
    assert_eq!(after.registry_live_records, before.registry_live_records);
    assert_eq!(
        after.registry_physical_records,
        before.registry_physical_records
    );
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
    assert!(stats.nonempty_hash_bucket_count <= stats.hash_bucket_count);
    assert!(stats.nonempty_hash_bucket_count <= stats.node_used);
    assert_eq!(stats.chain_corruptions, 0);
    assert_eq!(
        stats
            .free_pages
            .saturating_add(stats.allocated_pages)
            .saturating_add(stats.reserved_pages),
        stats.total_pages
    );

    let audit = KERNEL_ALLOCATOR.audit();
    assert!(
        !audit
            .flags
            .contains(AllocatorAuditFlags::PHYS_PAGE_ACCOUNTING_MISMATCH)
    );
    assert!(audit.phys_structure_scanned);
    assert_eq!(audit.phys_structure.flags, BuddyAuditFlags::empty());

    let buddy_audit = KERNEL_ALLOCATOR.buddy_audit();
    assert!(buddy_audit.is_consistent());
    assert_eq!(buddy_audit.flags, BuddyAuditFlags::empty());
    assert_eq!(buddy_audit.scanned_total_pages, stats.total_pages);
    assert_eq!(buddy_audit.scanned_free_pages, stats.free_pages);
    assert_eq!(buddy_audit.scanned_allocated_pages, stats.allocated_pages);
    assert_eq!(buddy_audit.scanned_reserved_pages, stats.reserved_pages);
    assert_eq!(
        buddy_audit
            .scanned_hash_nodes
            .saturating_add(buddy_audit.scanned_recycled_nodes),
        stats.node_used
    );
    assert_eq!(
        buddy_audit.scanned_nonempty_hash_buckets,
        stats.nonempty_hash_bucket_count
    );

    let metadata = KERNEL_ALLOCATOR.metadata_stats();
    assert!(metadata.backing_pages > 0);
    assert!(metadata.dynamic_allocations > 0);
}
