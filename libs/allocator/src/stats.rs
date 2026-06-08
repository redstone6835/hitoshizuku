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
use crate::registry::AllocationRegistryStats;
use crate::slab::SlabStats;
use crate::space::AddressSpaceStats;

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

pub fn format_diagnostic(
    buf: &mut [u8],
    overview: &MemoryOverview,
    layers: &AllocatorLayerStats,
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
    pos += write_str(buf, pos, b" reclaim=");
    pos += write_u64(buf, pos, layers.slab.reclaimed_slabs);
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
    pos += write_str(buf, pos, b"\nReg: live=");
    pos += write_usize(buf, pos, layers.registry.live_records);
    pos += write_str(buf, pos, b" max_chain=");
    pos += write_usize(buf, pos, layers.registry.max_chain_len);
    pos += write_str(buf, pos, b" dup=");
    pos += write_u64(buf, pos, layers.registry.duplicate_inserts);
    pos += write_str(buf, pos, b" double_free=");
    pos += write_u64(buf, pos, layers.registry.double_free_attempts);
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
