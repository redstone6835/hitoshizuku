//! 分配器统计与总览视图。
//!
//! allocator 实际上由多层组成：boot、buddy、vmem、slab、kernel heap、managed。
//! 单看某一层的统计数字通常是不够的，因为问题往往发生在层与层的交界处。例如：
//!
//! - 物理页还有很多，但内核虚拟地址空间已经耗尽；
//! - slab 命中率很好，但大对象路径失败；
//! - managed 区域未启用，却仍有上层代码误以为可用。
//!
//! 这个模块负责把分散的统计数据组织成较高层的视图，使内核日志和自检代码能够用
//! 一套统一格式描述当前内存状态。
use crate::boot::BootStats;
use crate::buddy::{BuddyStats, PAGE_SIZE};
use crate::gc::{GcCollectionKind, GcPhase};
use crate::kheap::KernelHeapStats;
use crate::managed::ManagedStats;
use crate::metadata::MetadataStats;
use crate::registry::{AllocationRegistryAudit, AllocationRegistryStats};
use crate::slab::SlabStats;
use crate::space::AddressSpaceStats;

/// allocator 分层账本审计结果。
///
/// 这个结构只使用各层已经维护的统计快照，不扫描对象内容，也不修复状态。它适合在测试、
/// bench 和故障日志中确认“registry 账本”和“实际后端计数”是否仍然对得上。由于这里不是
/// 全局停机快照，并发 alloc/free 期间可能观察到短暂中间态；严格判定应在 quiescent 状态下
/// 使用。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocatorAudit {
    pub flags: AllocatorAuditFlags,
    pub registry_structure: AllocationRegistryAudit,
    pub registry_live_records: usize,
    pub registry_kind_records: usize,
    pub registry_boot_records: usize,
    pub registry_physical_records: usize,
    pub registry_node_capacity: usize,
    pub registry_nodes_accounted: usize,
    pub slab_active_objects: u64,
    pub slab_live_records: usize,
    pub slab_active_bytes: usize,
    pub slab_backing_bytes: usize,
    pub kheap_active_allocs: u64,
    pub kheap_live_records: usize,
    pub kheap_active_bytes: usize,
    pub kheap_page_bytes: usize,
    pub managed_active_objects: u64,
    pub managed_live_records: usize,
}

impl AllocatorAudit {
    pub const fn is_consistent(self) -> bool {
        self.flags.is_empty() && self.registry_structure.is_consistent()
    }
}

/// allocator 审计发现的问题集合。
///
/// 使用位标志而不是字符串，是为了让内核自检、benchmark 和未来外部扩展都能用类型安全的
/// 方式判断具体问题，不需要解析日志文本。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocatorAuditFlags(u32);

impl AllocatorAuditFlags {
    pub const REGISTRY_KIND_MISMATCH: Self = Self(1 << 0);
    pub const REGISTRY_NODE_ACCOUNTING_MISMATCH: Self = Self(1 << 1);
    /// 兼容旧命名。节点池少记或多记都会破坏 allocator 账本完整性，因此新代码应使用
    /// [`AllocatorAuditFlags::REGISTRY_NODE_ACCOUNTING_MISMATCH`]。
    pub const REGISTRY_NODE_ACCOUNTING_OVERFLOW: Self = Self::REGISTRY_NODE_ACCOUNTING_MISMATCH;
    pub const SLAB_RECORD_MISMATCH: Self = Self(1 << 2);
    pub const SLAB_BACKING_OVERCOMMIT: Self = Self(1 << 3);
    pub const KHEAP_RECORD_MISMATCH: Self = Self(1 << 4);
    pub const KHEAP_PAGE_ACCOUNTING_MISMATCH: Self = Self(1 << 5);
    pub const MANAGED_RECORD_MISMATCH: Self = Self(1 << 6);
    pub const REGISTRY_STRUCTURE_MISMATCH: Self = Self(1 << 7);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, flag: Self) -> bool {
        (self.0 & flag.0) != 0
    }

    fn insert(&mut self, flag: Self) {
        self.0 |= flag.0;
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryOverview {
    pub total_physical: usize,
    pub allocated_physical: usize,
    pub free_physical: usize,
    pub reserved_physical: usize,
    pub direct_map_total: usize,
    pub direct_map_allocated: usize,
    pub direct_map_free: usize,
    pub kernel_vmem_total: usize,
    pub kernel_vmem_allocated: usize,
    pub kernel_vmem_free: usize,
    pub managed_vmem_total: usize,
    pub managed_vmem_allocated: usize,
    pub managed_vmem_free: usize,
    pub kernel_heap_used: usize,
    pub kernel_heap_free: usize,
    pub boot_used: usize,
    pub boot_free: usize,
    pub pressure_level: u8,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AllocatorLayerStats {
    pub phys: BuddyStats,
    pub address_space: AddressSpaceStats,
    pub kheap: KernelHeapStats,
    pub slab: SlabStats,
    pub metadata: MetadataStats,
    pub registry: AllocationRegistryStats,
    pub managed: ManagedStats,
}

/// allocator 热点摘要。
///
/// 这个结构只从已有统计快照派生，不扫描对象、不分配内存。它面向 benchmark、诊断日志和
/// 未来外部扩展：调用方可以直接读取类型化字段判断当前主要瓶颈，而不是解析一段成本模型
/// 文本。例如 slab cache 命中率下降时应优先看 per-CPU cache/refill，registry 链变长时应
/// 优先看 bucket/shard 参数，vmem 最大空闲段过小时则说明大对象路径受碎片化影响。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocatorHotspotSummary {
    pub slab_cache_hit_per_mille: u16,
    pub slab_cache_miss_per_mille: u16,
    pub slab_refill_per_mille: u16,
    pub slab_flush_per_mille: u16,
    pub slab_fast_free_per_mille: u16,
    pub slab_fast_free_fallbacks: u64,
    pub registry_max_chain_len: usize,
    pub registry_max_shard_live_records: usize,
    pub registry_live_per_bucket_per_mille: u16,
    pub registry_nonempty_buckets: usize,
    pub registry_nonempty_load_per_mille: u16,
    pub registry_max_shard_nonempty_buckets: usize,
    pub registry_underflows: u64,
    pub kheap_failure_per_mille: u16,
    pub kheap_realloc_per_mille: u16,
    pub kheap_cache_hit_per_mille: u16,
    pub kheap_cached_pages: usize,
    pub kheap_cache_pressure_releases: u64,
    pub kernel_vmem_largest_free_percent: u8,
    pub kernel_vmem_free_segments: usize,
    pub managed_fragmentation_per_mille: u16,
    pub pressure_level: u8,
}

pub fn build_audit(
    layers: &AllocatorLayerStats,
    registry_structure: AllocationRegistryAudit,
) -> AllocatorAudit {
    let registry_kind_records = layers
        .registry
        .live_boot
        .saturating_add(layers.registry.live_small)
        .saturating_add(layers.registry.live_large)
        .saturating_add(layers.registry.live_managed)
        .saturating_add(layers.registry.live_physical);
    let registry_nodes_accounted = layers
        .registry
        .live_records
        .saturating_add(layers.registry.free_nodes);
    let slab_backing_bytes = pages_to_bytes(layers.slab.active_pages);
    let kheap_page_bytes = pages_to_bytes(layers.kheap.active_pages);

    let mut flags = AllocatorAuditFlags::empty();
    if registry_kind_records != layers.registry.live_records {
        flags.insert(AllocatorAuditFlags::REGISTRY_KIND_MISMATCH);
    }
    if registry_nodes_accounted != layers.registry.nodes_allocated {
        flags.insert(AllocatorAuditFlags::REGISTRY_NODE_ACCOUNTING_MISMATCH);
    }
    if !registry_structure.is_consistent()
        || registry_structure.scanned_live_records != layers.registry.live_records
        || registry_structure.scanned_free_nodes != layers.registry.free_nodes
        || registry_structure.scanned_live_boot != layers.registry.live_boot
        || registry_structure.scanned_live_small != layers.registry.live_small
        || registry_structure.scanned_live_large != layers.registry.live_large
        || registry_structure.scanned_live_managed != layers.registry.live_managed
        || registry_structure.scanned_live_physical != layers.registry.live_physical
        || registry_structure.scanned_nonempty_buckets != layers.registry.nonempty_buckets
        || registry_structure.scanned_max_chain_len > layers.registry.max_chain_len
    {
        flags.insert(AllocatorAuditFlags::REGISTRY_STRUCTURE_MISMATCH);
    }
    if layers.slab.active_objects != layers.registry.live_small as u64 {
        flags.insert(AllocatorAuditFlags::SLAB_RECORD_MISMATCH);
    }
    if layers.slab.active_bytes > slab_backing_bytes {
        flags.insert(AllocatorAuditFlags::SLAB_BACKING_OVERCOMMIT);
    }
    if layers.kheap.active_allocs != layers.registry.live_large as u64 {
        flags.insert(AllocatorAuditFlags::KHEAP_RECORD_MISMATCH);
    }
    if layers.kheap.active_bytes != kheap_page_bytes {
        flags.insert(AllocatorAuditFlags::KHEAP_PAGE_ACCOUNTING_MISMATCH);
    }
    if layers.managed.active_objects != layers.registry.live_managed as u64 {
        flags.insert(AllocatorAuditFlags::MANAGED_RECORD_MISMATCH);
    }

    AllocatorAudit {
        flags,
        registry_structure,
        registry_live_records: layers.registry.live_records,
        registry_kind_records,
        registry_boot_records: layers.registry.live_boot,
        registry_physical_records: layers.registry.live_physical,
        registry_node_capacity: layers.registry.nodes_allocated,
        registry_nodes_accounted,
        slab_active_objects: layers.slab.active_objects,
        slab_live_records: layers.registry.live_small,
        slab_active_bytes: layers.slab.active_bytes,
        slab_backing_bytes,
        kheap_active_allocs: layers.kheap.active_allocs,
        kheap_live_records: layers.registry.live_large,
        kheap_active_bytes: layers.kheap.active_bytes,
        kheap_page_bytes,
        managed_active_objects: layers.managed.active_objects,
        managed_live_records: layers.registry.live_managed,
    }
}

pub fn build_hotspot_summary(layers: &AllocatorLayerStats) -> AllocatorHotspotSummary {
    let slab_total = layers
        .slab
        .cache_hits
        .saturating_add(layers.slab.cache_misses);
    let registry_live_per_bucket = if layers.registry.bucket_count == 0 {
        0
    } else {
        per_mille(
            layers.registry.live_records as u64,
            layers.registry.bucket_count as u64,
        )
    };
    let registry_nonempty_load = if layers.registry.bucket_count == 0 {
        0
    } else {
        per_mille(
            layers.registry.nonempty_buckets as u64,
            layers.registry.bucket_count as u64,
        )
    };
    let kheap_requests = layers.kheap.alloc_requests;
    let kheap_activity = layers
        .kheap
        .alloc_requests
        .saturating_add(layers.kheap.realloc_requests);
    let kheap_cache_lookups = layers
        .kheap
        .cache_hits
        .saturating_add(layers.kheap.cache_misses);
    let kernel_vmem_largest_free_percent = if layers.address_space.kernel.total_size == 0 {
        0
    } else {
        ((layers.address_space.kernel.largest_free_size as u128 * 100)
            / layers.address_space.kernel.total_size as u128)
            .min(100) as u8
    };

    AllocatorHotspotSummary {
        slab_cache_hit_per_mille: per_mille(layers.slab.cache_hits, slab_total),
        slab_cache_miss_per_mille: per_mille(layers.slab.cache_misses, slab_total),
        slab_refill_per_mille: per_mille(layers.slab.cache_refills, slab_total),
        slab_flush_per_mille: per_mille(layers.slab.cache_flushes, slab_total),
        slab_fast_free_per_mille: per_mille(layers.slab.fast_free_hits, layers.slab.free_requests),
        slab_fast_free_fallbacks: layers.slab.fast_free_fallbacks,
        registry_max_chain_len: layers.registry.max_chain_len,
        registry_max_shard_live_records: layers.registry.max_shard_live_records,
        registry_live_per_bucket_per_mille: registry_live_per_bucket,
        registry_nonempty_buckets: layers.registry.nonempty_buckets,
        registry_nonempty_load_per_mille: registry_nonempty_load,
        registry_max_shard_nonempty_buckets: layers.registry.max_shard_nonempty_buckets,
        registry_underflows: layers.registry.accounting_underflows,
        kheap_failure_per_mille: per_mille(layers.kheap.alloc_failures, kheap_requests),
        kheap_realloc_per_mille: per_mille(layers.kheap.realloc_requests, kheap_activity),
        kheap_cache_hit_per_mille: per_mille(layers.kheap.cache_hits, kheap_cache_lookups),
        kheap_cached_pages: layers.kheap.cached_pages,
        kheap_cache_pressure_releases: layers.kheap.cache_pressure_releases,
        kernel_vmem_largest_free_percent,
        kernel_vmem_free_segments: layers.address_space.kernel.free_segments,
        managed_fragmentation_per_mille: layers.managed.gc.fragmentation_ratio.min(1000) as u16,
        pressure_level: pressure_level(layers.phys, layers.address_space, layers.managed),
    }
}

pub fn build_overview(
    boot: BootStats,
    phys: BuddyStats,
    address_space: AddressSpaceStats,
    kheap: KernelHeapStats,
    slab: SlabStats,
    managed: ManagedStats,
) -> MemoryOverview {
    let free_physical = pages_to_bytes(phys.free_pages);
    let kernel_heap_used = kheap.active_bytes.saturating_add(slab.active_bytes);
    MemoryOverview {
        total_physical: pages_to_bytes(phys.total_pages),
        allocated_physical: pages_to_bytes(phys.allocated_pages),
        free_physical,
        reserved_physical: pages_to_bytes(phys.reserved_pages),
        direct_map_total: address_space.direct_map.total_size,
        direct_map_allocated: address_space.direct_map.allocated_size,
        direct_map_free: address_space.direct_map.free_size,
        kernel_vmem_total: address_space.kernel.total_size,
        kernel_vmem_allocated: address_space.kernel.allocated_size,
        kernel_vmem_free: address_space.kernel.free_size,
        managed_vmem_total: address_space.managed.total_size,
        managed_vmem_allocated: address_space.managed.allocated_size,
        managed_vmem_free: address_space.managed.free_size,
        kernel_heap_used,
        kernel_heap_free: address_space.kernel.free_size,
        boot_used: boot.used_bytes,
        boot_free: boot.free_bytes,
        pressure_level: pressure_level(phys, address_space, managed),
    }
}

pub fn build_overview_from_layers(boot: BootStats, layers: &AllocatorLayerStats) -> MemoryOverview {
    // 诊断路径已经需要完整 layer snapshot；从同一份快照派生 overview 可以避免重复读取
    // phys/vmem/slab/kheap/managed，也让日志中总览和分层数据对应同一个采样窗口。
    build_overview(
        boot,
        layers.phys,
        layers.address_space,
        layers.kheap,
        layers.slab,
        layers.managed,
    )
}

#[inline]
fn pages_to_bytes(pages: usize) -> usize {
    pages.saturating_mul(PAGE_SIZE)
}

pub fn pressure_level(
    phys: BuddyStats,
    address_space: AddressSpaceStats,
    managed: ManagedStats,
) -> u8 {
    let phys_pressure = physical_pressure_level(phys);
    let mut managed_pressure = 0u8;
    if managed.enabled {
        let managed_free_percent = if address_space.managed.total_size == 0 {
            100
        } else {
            (address_space.managed.free_size * 100) / address_space.managed.total_size
        };
        let object_load = if managed.gc.object_table_capacity == 0 {
            0
        } else {
            (managed.gc.object_table_entries * 100) / managed.gc.object_table_capacity
        };
        if managed_free_percent < 5 || object_load >= 95 {
            managed_pressure = 3;
        } else if managed_free_percent < 10 || object_load >= 85 {
            managed_pressure = 2;
        } else if managed_free_percent < 25 || object_load >= 70 {
            managed_pressure = 1;
        }
    }
    phys_pressure.max(managed_pressure)
}

fn physical_pressure_level(phys: BuddyStats) -> u8 {
    if phys.total_pages == 0 {
        return 0;
    }
    let free_percent = (phys.free_pages * 100) / phys.total_pages;
    if free_percent < 5 {
        3
    } else if free_percent < 10 {
        2
    } else if free_percent < 25 {
        1
    } else {
        0
    }
}

#[inline]
fn per_mille(part: u64, total: u64) -> u16 {
    if total == 0 {
        return 0;
    }
    ((part as u128 * 1000) / total as u128).min(1000) as u16
}

pub fn format_diagnostic(
    buf: &mut [u8],
    overview: &MemoryOverview,
    layers: &AllocatorLayerStats,
    registry_structure: &AllocationRegistryAudit,
) -> usize {
    let mut pos = 0usize;
    pos += write_str(buf, pos, b"Phys: total=");
    pos += write_usize(buf, pos, overview.total_physical / 1024);
    pos += write_str(buf, pos, b"K free=");
    pos += write_usize(buf, pos, overview.free_physical / 1024);
    pos += write_str(buf, pos, b"K pressure=");
    pos += write_usize(buf, pos, overview.pressure_level as usize);
    pos += write_str(buf, pos, b" meta=");
    pos += write_usize(buf, pos, pages_to_bytes(layers.phys.metadata_pages) / 1024);
    pos += write_str(buf, pos, b"K nodes=");
    pos += write_usize(buf, pos, layers.phys.node_used);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, layers.phys.node_capacity);
    pos += write_str(buf, pos, b" buckets=");
    pos += write_usize(buf, pos, layers.phys.hash_bucket_count);
    pos += write_str(buf, pos, b"\nAddr: dm=");
    pos += write_usize(buf, pos, overview.direct_map_allocated / 1024);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, overview.direct_map_total / 1024);
    pos += write_str(buf, pos, b"K kernel=");
    pos += write_usize(buf, pos, overview.kernel_vmem_allocated / 1024);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, overview.kernel_vmem_total / 1024);
    pos += write_str(buf, pos, b"K managed=");
    pos += write_usize(buf, pos, overview.managed_vmem_allocated / 1024);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, overview.managed_vmem_total / 1024);
    pos += write_str(buf, pos, b"K largest=");
    pos += write_usize(
        buf,
        pos,
        layers.address_space.kernel.largest_free_size / 1024,
    );
    pos += write_str(buf, pos, b"K free_segs=");
    pos += write_usize(buf, pos, layers.address_space.kernel.free_segments);
    pos += write_str(buf, pos, b" tags=");
    pos += write_usize(buf, pos, layers.address_space.kernel.active_tags);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, layers.address_space.kernel.free_tags);
    pos += write_str(buf, pos, b" tag_refill=");
    pos += write_u64(buf, pos, layers.address_space.kernel.tag_refills);
    pos += write_str(buf, pos, b"K\nSlab: alloc=");
    pos += write_u64(buf, pos, layers.slab.alloc_requests);
    pos += write_str(buf, pos, b" free=");
    pos += write_u64(buf, pos, layers.slab.free_requests);
    pos += write_str(buf, pos, b" hit=");
    pos += write_u64(buf, pos, layers.slab.cache_hits);
    pos += write_str(buf, pos, b" grow_fail=");
    pos += write_u64(buf, pos, layers.slab.grow_failures);
    pos += write_str(buf, pos, b" refill=");
    pos += write_u64(buf, pos, layers.slab.cache_refills);
    pos += write_str(buf, pos, b" flush=");
    pos += write_u64(buf, pos, layers.slab.cache_flushes);
    pos += write_str(buf, pos, b" fast_free=");
    pos += write_u64(buf, pos, layers.slab.fast_free_hits);
    pos += write_str(buf, pos, b" fallback=");
    pos += write_u64(buf, pos, layers.slab.fast_free_fallbacks);
    pos += write_str(buf, pos, b" reclaim=");
    pos += write_u64(buf, pos, layers.slab.reclaimed_slabs);
    pos += write_str(buf, pos, b" free_nodes=");
    pos += write_usize(buf, pos, layers.slab.free_slab_nodes);
    pos += write_str(buf, pos, b"\nMeta: backing=");
    pos += write_usize(
        buf,
        pos,
        pages_to_bytes(layers.metadata.backing_pages) / 1024,
    );
    pos += write_str(buf, pos, b"K used=");
    pos += write_usize(buf, pos, layers.metadata.allocated_bytes / 1024);
    pos += write_str(buf, pos, b"K boot=");
    pos += write_u64(buf, pos, layers.metadata.boot_allocations);
    pos += write_str(buf, pos, b" dyn=");
    pos += write_u64(buf, pos, layers.metadata.dynamic_allocations);
    pos += write_str(buf, pos, b"\nKHeap: alloc=");
    pos += write_u64(buf, pos, layers.kheap.alloc_requests);
    pos += write_str(buf, pos, b" free=");
    pos += write_u64(buf, pos, layers.kheap.free_requests);
    pos += write_str(buf, pos, b" fail=");
    pos += write_u64(buf, pos, layers.kheap.alloc_failures);
    pos += write_str(buf, pos, b" cache_hit=");
    pos += write_u64(buf, pos, layers.kheap.cache_hits);
    pos += write_str(buf, pos, b" cache_miss=");
    pos += write_u64(buf, pos, layers.kheap.cache_misses);
    pos += write_str(buf, pos, b" cached=");
    pos += write_usize(buf, pos, layers.kheap.cached_ranges);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, layers.kheap.cached_pages);
    pos += write_str(buf, pos, b" pressure_rel=");
    pos += write_u64(buf, pos, layers.kheap.cache_pressure_releases);
    pos += write_str(buf, pos, b"\nReg: live=");
    pos += write_usize(buf, pos, layers.registry.live_records);
    pos += write_str(buf, pos, b" shards=");
    pos += write_usize(buf, pos, layers.registry.shard_count);
    pos += write_str(buf, pos, b" max_shard=");
    pos += write_usize(buf, pos, layers.registry.max_shard_live_records);
    pos += write_str(buf, pos, b" free=");
    pos += write_usize(buf, pos, layers.registry.free_nodes);
    pos += write_str(buf, pos, b" max_chain=");
    pos += write_usize(buf, pos, layers.registry.max_chain_len);
    pos += write_str(buf, pos, b" nonempty=");
    pos += write_usize(buf, pos, layers.registry.nonempty_buckets);
    pos += write_str(buf, pos, b" max_nonempty=");
    pos += write_usize(buf, pos, layers.registry.max_shard_nonempty_buckets);
    pos += write_str(buf, pos, b" refill=");
    pos += write_u64(buf, pos, layers.registry.node_refills);
    pos += write_str(buf, pos, b" nodes=");
    pos += write_usize(buf, pos, layers.registry.nodes_allocated);
    pos += write_str(buf, pos, b" dup=");
    pos += write_u64(buf, pos, layers.registry.duplicate_inserts);
    pos += write_str(buf, pos, b" double_free=");
    pos += write_u64(buf, pos, layers.registry.double_free_attempts);
    pos += write_str(buf, pos, b" underflow=");
    pos += write_u64(buf, pos, layers.registry.accounting_underflows);
    let hotspot = build_hotspot_summary(layers);
    pos += write_str(buf, pos, b"\nHot: slab_hit=");
    pos += write_usize(buf, pos, hotspot.slab_cache_hit_per_mille as usize);
    pos += write_str(buf, pos, b" slab_miss=");
    pos += write_usize(buf, pos, hotspot.slab_cache_miss_per_mille as usize);
    pos += write_str(buf, pos, b" slab_fast_free=");
    pos += write_usize(buf, pos, hotspot.slab_fast_free_per_mille as usize);
    pos += write_str(buf, pos, b" slab_fallback=");
    pos += write_u64(buf, pos, hotspot.slab_fast_free_fallbacks);
    pos += write_str(buf, pos, b" reg_chain=");
    pos += write_usize(buf, pos, hotspot.registry_max_chain_len);
    pos += write_str(buf, pos, b" reg_load=");
    pos += write_usize(
        buf,
        pos,
        hotspot.registry_live_per_bucket_per_mille as usize,
    );
    pos += write_str(buf, pos, b" reg_nonempty=");
    pos += write_usize(buf, pos, hotspot.registry_nonempty_buckets);
    pos += write_str(buf, pos, b" reg_nonempty_load=");
    pos += write_usize(buf, pos, hotspot.registry_nonempty_load_per_mille as usize);
    pos += write_str(buf, pos, b" kheap_fail=");
    pos += write_usize(buf, pos, hotspot.kheap_failure_per_mille as usize);
    pos += write_str(buf, pos, b" kheap_cache=");
    pos += write_usize(buf, pos, hotspot.kheap_cache_hit_per_mille as usize);
    pos += write_str(buf, pos, b" kheap_cached_pages=");
    pos += write_usize(buf, pos, hotspot.kheap_cached_pages);
    pos += write_str(buf, pos, b" vmem_largest=");
    pos += write_usize(buf, pos, hotspot.kernel_vmem_largest_free_percent as usize);
    let audit = build_audit(layers, *registry_structure);
    pos += write_str(buf, pos, b"\nAudit: ok=");
    pos += write_usize(buf, pos, audit.is_consistent() as usize);
    pos += write_str(buf, pos, b" flags=");
    pos += write_usize(buf, pos, audit.flags.bits() as usize);
    pos += write_str(buf, pos, b" live=");
    pos += write_usize(buf, pos, audit.registry_live_records);
    pos += write_str(buf, pos, b" kinds=");
    pos += write_usize(buf, pos, audit.registry_kind_records);
    pos += write_str(buf, pos, b" boot=");
    pos += write_usize(buf, pos, audit.registry_boot_records);
    pos += write_str(buf, pos, b" physrec=");
    pos += write_usize(buf, pos, audit.registry_physical_records);
    pos += write_str(buf, pos, b" nodes=");
    pos += write_usize(buf, pos, audit.registry_nodes_accounted);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, audit.registry_node_capacity);
    pos += write_str(buf, pos, b" reg_struct=");
    pos += write_usize(buf, pos, audit.registry_structure.flags.bits() as usize);
    pos += write_str(buf, pos, b" scan=");
    pos += write_usize(buf, pos, audit.registry_structure.scanned_live_records);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, audit.registry_structure.scanned_free_nodes);
    pos += write_str(buf, pos, b" chain=");
    pos += write_usize(buf, pos, audit.registry_structure.scanned_max_chain_len);
    pos += write_str(buf, pos, b" slab=");
    pos += write_u64(buf, pos, audit.slab_active_objects);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, audit.slab_live_records);
    pos += write_str(buf, pos, b" kheap=");
    pos += write_u64(buf, pos, audit.kheap_active_allocs);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, audit.kheap_live_records);
    pos += write_str(buf, pos, b" managed=");
    pos += write_u64(buf, pos, audit.managed_active_objects);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, audit.managed_live_records);
    pos += write_str(buf, pos, b"\nGC: enabled=");
    pos += write_usize(buf, pos, layers.managed.enabled as usize);
    pos += write_str(buf, pos, b" major=");
    pos += write_u64(buf, pos, layers.managed.gc.major_gc_count);
    pos += write_str(buf, pos, b" minor=");
    pos += write_u64(buf, pos, layers.managed.gc.minor_gc_count);
    pos += write_str(buf, pos, b" phase=");
    pos += write_str(
        buf,
        pos,
        gc_phase_code(
            layers
                .managed
                .gc_control
                .map_or(GcPhase::Idle, |snapshot| snapshot.phase),
        ),
    );
    pos += write_str(buf, pos, b" kind=");
    pos += write_str(
        buf,
        pos,
        gc_kind_name(layers.managed.gc.last_collection_kind),
    );
    pos += write_str(buf, pos, b" objects=");
    pos += write_usize(buf, pos, layers.managed.gc.object_table_entries);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, layers.managed.gc.object_table_capacity);
    pos += write_str(buf, pos, b" y/o=");
    pos += write_u64(buf, pos, layers.managed.gc.young_gen_objects);
    pos += write_str(buf, pos, b"/");
    pos += write_u64(buf, pos, layers.managed.gc.old_gen_objects);
    pos += write_str(buf, pos, b" wb=");
    pos += write_u64(buf, pos, layers.managed.gc.write_barrier_count);
    pos += write_str(buf, pos, b" cards=");
    pos += write_usize(buf, pos, layers.managed.gc.dirty_cards);
    pos += write_str(buf, pos, b" rem=");
    pos += write_usize(buf, pos, layers.managed.gc.remembered_objects);
    pos += write_str(buf, pos, b" inc=");
    pos += write_u64(buf, pos, layers.managed.gc.incremental_mark_steps);
    pos += write_str(buf, pos, b" moved=");
    pos += write_u64(buf, pos, layers.managed.gc.objects_compacted);
    pos += write_str(buf, pos, b" handles=");
    pos += write_usize(buf, pos, layers.managed.gc.strong_handle_slots);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, layers.managed.gc.weak_handle_slots);
    pos += write_str(buf, pos, b"/");
    pos += write_usize(buf, pos, layers.managed.gc.pinned_handle_slots);
    pos += write_str(buf, pos, b" pending_fin=");
    pos += write_usize(buf, pos, layers.managed.gc.pending_finalizers);
    pos += write_str(buf, pos, b"\n");
    pos.min(buf.len())
}

fn gc_phase_code(phase: GcPhase) -> &'static [u8] {
    match phase {
        GcPhase::Idle => b"idle",
        GcPhase::RootScan => b"roots",
        GcPhase::InitialMark => b"init",
        GcPhase::MarkPropagate => b"mark",
        GcPhase::Remark => b"remark",
        GcPhase::Sweep => b"sweep",
        GcPhase::Compact => b"compact",
        GcPhase::Finalize => b"fin",
    }
}

fn gc_kind_name(kind: GcCollectionKind) -> &'static [u8] {
    match kind {
        GcCollectionKind::None => b"none",
        GcCollectionKind::IncrementalMark => b"inc",
        GcCollectionKind::Minor => b"minor",
        GcCollectionKind::Major => b"major",
    }
}

fn write_str(buf: &mut [u8], pos: usize, s: &[u8]) -> usize {
    let mut written = 0;
    while pos + written < buf.len() && written < s.len() {
        buf[pos + written] = s[written];
        written += 1;
    }
    written
}

fn write_u64(buf: &mut [u8], pos: usize, value: u64) -> usize {
    write_usize(buf, pos, value as usize)
}

fn write_usize(buf: &mut [u8], pos: usize, mut value: usize) -> usize {
    let mut tmp = [0u8; 32];
    let mut len = 0;
    loop {
        tmp[len] = b'0' + (value % 10) as u8;
        len += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }

    let mut written = 0;
    while written < len && pos + written < buf.len() {
        buf[pos + written] = tmp[len - written - 1];
        written += 1;
    }
    written
}
