//! 分层性能基准测试模块（控制变量法）。
//!
//! 设计思路：
//! 从最底层的硬件/软件原语开始，逐步叠加抽象层，每一步都测量相同的操作（相同数据量、
//! 相同访问模式），仅改变被测层。通过对比不同层的测试结果，可以直观地
//! 看出性能瓶颈究竟位于块设备驱动、文件系统逻辑、VFS 层还是缓存缺失。
//!
//! 测试分层：
//!   L0  – 纯内存拷贝（理论带宽上限）
//!   L1  – 裸块设备顺序读写（块层软件开销）
//!   L2  – 裸块设备随机读取（小块IO延迟）
//!   L3/L4 – FAT32 / ext4 文件系统挂载
//!   L5  – 顺序读文件（1 MiB 块）
//!   L6  – 随机读文件（4 KiB 块）
//!   L7  – 顺序写文件（新建文件，无缓存）
//!   L8  – 元数据操作（readdir、创建/删除文件）

#[cfg(any(feature = "bench", feature = "block-bench"))]
use alloc::boxed::Box;
#[cfg(feature = "block-bench")]
use alloc::string::String;
#[cfg(any(feature = "bench", feature = "block-bench"))]
use alloc::sync::Arc;
#[cfg(any(feature = "bench", feature = "block-bench"))]
use alloc::vec;
#[cfg(any(feature = "bench", feature = "block-bench"))]
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
#[cfg(feature = "bench")]
use core::any::Any;
#[cfg(feature = "bench")]
use core::num::NonZeroU32;
#[cfg(any(feature = "bench", feature = "block-bench"))]
use core::ops::ControlFlow;

use allocator::{
    AllocatorAuditScope, AllocatorReclaimRequest, KERNEL_ALLOCATOR, MAX_SMALL_SIZE, MemoryDomain,
    MemoryPlacement, MemoryRequest, PAGE_SIZE, PhysicalAllocRequest, ReclaimPolicy, Zeroing,
};
#[cfg(any(feature = "bench", feature = "block-bench"))]
use vfs::cred::Credentials;
#[cfg(feature = "block-bench")]
use vfs::file::DirEntry;
#[cfg(any(feature = "bench", feature = "block-bench"))]
use vfs::file::OpenOptions;
#[cfg(feature = "bench")]
use vfs::superblock::FsDriver;
#[cfg(any(feature = "bench", feature = "block-bench"))]
use vfs::superblock::Superblock;
#[cfg(feature = "bench")]
use vfs::sync::Spinlock;

#[cfg(feature = "bench")]
use general::dev::bio::{Bio, BioIoError, BioResult, SubmitError};
#[cfg(any(feature = "bench", feature = "block-bench"))]
use general::dev::bio::{BioBuffer, BioOp, BlockRange};
#[cfg(feature = "bench")]
use general::dev::block::{BlockClass, BlockDeviceInit, BlockDriver, BlockGeometry, BlockLimits};
#[cfg(any(feature = "bench", feature = "block-bench"))]
use general::dev::block::{BlockDevice, BlockFeatures};
#[cfg(any(feature = "bench", feature = "block-bench"))]
use general::dev::block_sync::SyncBlockBackend;
#[cfg(any(feature = "bench", feature = "block-bench"))]
use general::dev::completion::Completion;
#[cfg(feature = "block-bench")]
use general::dev::control::{BlockControlRequest, BlockControlResponse};
#[cfg(feature = "block-bench")]
use general::dev::enumerate::DEVICES;
#[cfg(feature = "block-bench")]
use general::vfs::device_files::projection::active_block_devices;

// ── 嵌入的磁盘镜像 ──────────────────────────────────────────────────────

#[cfg(feature = "bench")]
static FAT_IMG: &[u8] = include_bytes!("../../build/fat32.img");
#[cfg(feature = "bench")]
static EXT_IMG: &[u8] = include_bytes!("../../build/ext4.img");

// ── 测试入口 ────────────────────────────────────────────────────────────

#[cfg(feature = "bench")]
pub fn run() {
    log::info!("[bench] ================= LAYERED PERF TEST =================");

    run_allocator_bench();
    run_memcpy_baseline();
    run_memcpy_cold();
    run_software_overhead_only();

    {
        let raw_dev = make_ram_device("ramd-raw", EXT_IMG);
        run_block_seq_read("ram-raw", &raw_dev);
        run_block_seq_write("ram-raw", &raw_dev);
        run_block_seq_read_instrumented("ram-raw", &raw_dev);
        run_block_overhead_breakdown("ram-raw", &raw_dev);
        run_block_rand_read("ram-raw", &raw_dev);

        let fat_dev = make_ram_device("ramd-fat", FAT_IMG);
        let ext_dev = make_ram_device("ramd-ext", EXT_IMG);
        let fat_sb = mount_fat("fat", Arc::clone(&fat_dev));
        let ext_sb = mount_ext("ext", Arc::clone(&ext_dev));

        // ─── L5/L7: 文件系统顺序写+读（先写后读，同一文件） ─────
        if let Some(ref sb) = fat_sb {
            run_fs_seq_write_read("fat", sb);
        }
        if let Some(ref sb) = ext_sb {
            run_fs_seq_write_read("ext", sb);
        }

        // ─── FAT 写路径细化插桩 ─────────────────────────────────
        if let Some(ref sb) = fat_sb {
            run_fat_write_breakdown("fat", sb);
        }

        // ─── EXT4 写路径细化插桩 ────────────────────────────────
        if let Some(ref sb) = ext_sb {
            run_ext_write_breakdown("ext", sb);
        }

        // ─── L6: 随机读文件 ─────────────────────────────────────
        if let Some(ref sb) = fat_sb {
            run_fs_rand_read("fat", sb, &fat_dev);
        }
        if let Some(ref sb) = ext_sb {
            run_fs_rand_read("ext", sb, &ext_dev);
        }

        // ─── L8: 元数据操作 ─────────────────────────────────────
        if let Some(ref sb) = fat_sb {
            run_fs_meta("fat", sb);
        }
        if let Some(ref sb) = ext_sb {
            run_fs_meta("ext", sb);
        }

        release_bench_superblock("fat", fat_sb);
        release_bench_superblock("ext", ext_sb);
        drop(raw_dev);
    }
    reclaim_allocator_caches_after_bench("full");

    log::info!("[bench] ================= TEST COMPLETE ====================");
}

#[cfg(feature = "block-bench")]
pub fn run_block_device() {
    log::info!("[bench] ============ BLOCK DEVICE PERF TEST ============");

    run_memcpy_baseline();
    run_memcpy_cold();

    let devices = active_block_devices(&DEVICES.functions);
    if devices.is_empty() {
        log::error!("[bench][block] no active block devices found");
        reclaim_allocator_caches_after_bench("block-device");
        log::info!("[bench] ================= TEST COMPLETE ====================");
        return;
    }

    let mut tested_fat = false;
    let mut tested_ext = false;

    for dev in devices {
        let dev_name = String::from(dev.name());
        log::info!(
            "[bench][block] candidate={} logical={} physical={} blocks={:?}",
            dev_name,
            dev.geometry().logical_block_size().get(),
            dev.geometry().physical_block_size().get(),
            dev.geometry().block_count()
        );

        let raw_tag = alloc::format!("block-{}", dev_name);
        let mut auto_mounted = false;
        match general::vfs::mount_block_device_auto(Arc::clone(&dev), "") {
            Ok((sb, source)) => {
                auto_mounted = true;
                log::info!(
                    "[bench][block] mounted candidate={} fs={} source={}",
                    dev_name,
                    sb.fs_type,
                    source
                );
                match sb.fs_type {
                    "fatfs" if !tested_fat => {
                        run_block_device_fs_read_suite("fat-block", &dev, &sb);
                        tested_fat = true;
                        release_bench_superblock_no_sync("fat-block", Some(sb));
                    }
                    "extfs" | "ext2" | "ext3" | "ext4" if !tested_ext => {
                        run_block_device_fs_read_suite("ext-block", &dev, &sb);
                        tested_ext = true;
                        release_bench_superblock_no_sync("ext-block", Some(sb));
                    }
                    _ => release_bench_superblock_no_sync(&raw_tag, Some(sb)),
                }
            }
            Err(err) => {
                log::warning!(
                    "[bench][block] candidate={} auto mount skipped: {:?}",
                    dev_name,
                    err
                );
            }
        }

        run_block_small_read(&raw_tag, &dev);
        run_block_rw_flush_validation(&raw_tag, &dev);
        if !auto_mounted {
            run_block_range_ops_validation(&raw_tag, &dev);
        }
        run_block_seq_read(&raw_tag, &dev);
        run_block_seq_read_repeat(&raw_tag, &dev);
        run_block_overhead_diagnosis(&raw_tag, &dev);
        run_block_rand_read(&raw_tag, &dev);
    }

    if !tested_fat {
        log::warning!("[bench][fat-block] no FAT block device found");
    }

    if !tested_ext {
        log::warning!("[bench][ext-block] no EXT block device found");
    }

    reclaim_allocator_caches_after_bench("block-device");
    log::info!("[bench] ================= TEST COMPLETE ====================");
}

pub fn run_allocator_only() {
    log::info!("[bench] ================= ALLOCATOR PERF TEST =================");
    run_allocator_bench();
    reclaim_allocator_caches_after_bench("allocator-only");
    log::info!("[bench] ================= TEST COMPLETE ======================");
}

fn reclaim_allocator_caches_after_bench(tag: &str) {
    let before = KERNEL_ALLOCATOR.layer_stats();
    match KERNEL_ALLOCATOR.reclaim_caches() {
        Ok(reclaim) => {
            let after = KERNEL_ALLOCATOR.layer_stats();
            log::info!(
                "[bench][{}][reclaim-final] bytes={} kheap_ranges={} kheap_pages={} slab_flush={} slab_slabs={} cached_kb {}->{} slab_pages {}->{}",
                tag,
                reclaim.reclaimed_bytes(),
                reclaim.kheap.released_ranges,
                reclaim.kheap.released_pages,
                reclaim.slab.flushed_cached_objects,
                reclaim.slab.reclaimed_slabs,
                before.kheap.cached_bytes / 1024,
                after.kheap.cached_bytes / 1024,
                before.slab.active_pages,
                after.slab.active_pages,
            );
        }
        Err(err) => {
            log::warning!("[bench][{}][reclaim-final] failed: {:?}", tag, err);
        }
    }
}

#[cfg(feature = "bench")]
fn release_bench_superblock(tag: &str, sb: Option<Arc<Superblock>>) {
    if let Some(sb) = sb {
        if let Err(err) = sb.sync() {
            log::warning!(
                "[bench][{}] final sync failed before release: {:?}",
                tag,
                err
            );
        }
        vfs::DCACHE.invalidate_subtree(&sb.root_dentry);
        sb.gc_inode_cache();
        drop(sb);
    }
}

#[cfg(feature = "block-bench")]
fn release_bench_superblock_no_sync(tag: &str, sb: Option<Arc<Superblock>>) {
    if let Some(sb) = sb {
        vfs::DCACHE.invalidate_subtree(&sb.root_dentry);
        sb.gc_inode_cache();
        drop(sb);
        log::debug!("[bench][{}] released read-only bench superblock", tag);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 基础辅助
// ═══════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════
// Allocator: 分配器路径基准
// ═══════════════════════════════════════════════════════════════════════

// TODO(alloc-stabilization): allocator-bench 已能在 QEMU 下给出单次 ns/op、p50/p95
// 小样本和主要计数器来源。后续应把优化前后 ns/op 对比固化到提交说明或独立报告中，
// 并补多 CPU、cache warm/cold 等可重复采样场景，避免只凭 host check 推断内核态性能。
const ALLOC_BENCH_BATCH: usize = 128;
const ALLOC_BENCH_SAMPLES: usize = 7;

fn run_allocator_bench() {
    log::info!("[bench][alloc] ---------------- allocator cost model ----------------");
    let audit_start = KERNEL_ALLOCATOR.audit();
    log::info!(
        "[bench][alloc] small_limit={} page_size={} batch={} audit_ok={} audit_flags={} live={}",
        MAX_SMALL_SIZE,
        PAGE_SIZE,
        ALLOC_BENCH_BATCH,
        audit_start.is_consistent(),
        audit_start.flags.bits(),
        audit_start.registry_live_records
    );
    log_allocator_hotspot("start");

    let slab8 = run_allocator_alloc_free_case("slab-8", 8, 8, 256, Zeroing::Uninitialized);
    let slab64 = run_allocator_alloc_free_case("slab-64", 64, 8, 256, Zeroing::Uninitialized);
    run_allocator_sampled_alloc_free_case("slab-64", 64, 8, 16, Zeroing::Uninitialized);
    let slab80_align64 =
        run_allocator_alloc_free_case("slab-80-align64", 80, 64, 192, Zeroing::Uninitialized);
    let slab1024 = run_allocator_alloc_free_case("slab-1024", 1024, 8, 128, Zeroing::Uninitialized);
    let zero1024 = run_allocator_alloc_free_case("slab-1024-zeroed", 1024, 8, 128, Zeroing::Zeroed);
    if zero1024 >= slab1024 {
        log::info!(
            "[bench][alloc][cost] zeroing adds about {} ns/op at 1024B; source=memset on hot slab object",
            zero1024 - slab1024
        );
    }
    run_allocator_split_alloc_free_case("slab-64-split", 64, 8, 256, Zeroing::Uninitialized);

    let large4k =
        run_allocator_alloc_free_case("kheap-4k", 4096, PAGE_SIZE, 64, Zeroing::Uninitialized);
    run_allocator_sampled_alloc_free_case("kheap-4k", 4096, PAGE_SIZE, 8, Zeroing::Uninitialized);
    let large8k =
        run_allocator_alloc_free_case("kheap-8k", 8192, PAGE_SIZE, 48, Zeroing::Uninitialized);
    let large64k = run_allocator_alloc_free_case(
        "kheap-64k",
        64 * 1024,
        PAGE_SIZE,
        16,
        Zeroing::Uninitialized,
    );
    run_allocator_split_alloc_free_case(
        "kheap-4k-split",
        4096,
        PAGE_SIZE,
        64,
        Zeroing::Uninitialized,
    );
    run_allocator_kheap_full_cache_reuse();
    run_allocator_reclaim_api();

    run_allocator_registry_lookup();
    run_allocator_sampled_registry_lookup();
    run_allocator_in_place_query();
    run_allocator_realloc_same_class();
    run_allocator_typed_realloc_same_class();
    run_allocator_realloc_grow();
    run_allocator_counter_audit();
    run_allocator_audit();
    run_allocator_counter_diagnostic();
    run_allocator_diagnostic();
    run_allocator_physical();
    run_allocator_physical_exact_reclaim();

    let audit_end = KERNEL_ALLOCATOR.audit();
    if audit_end.is_consistent() {
        log::info!(
            "[bench][alloc][audit] final ok live={} boot={} physrec={} slab={}/{} kheap={}/{} managed={}/{}",
            audit_end.registry_live_records,
            audit_end.registry_boot_records,
            audit_end.registry_physical_records,
            audit_end.slab_active_objects,
            audit_end.slab_live_records,
            audit_end.kheap_active_allocs,
            audit_end.kheap_live_records,
            audit_end.managed_active_objects,
            audit_end.managed_live_records
        );
    } else {
        log::error!(
            "[bench][alloc][audit] final inconsistent flags={} reg_struct={} phys_struct={} slab_struct={} kheap_struct={} managed_struct={} live={} kinds={} boot={} physrec={} nodes={}/{} scan={}/{} slab={}/{} kheap={}/{} managed={}/{}",
            audit_end.flags.bits(),
            audit_end.registry_structure.flags.bits(),
            audit_end.phys_structure.flags.bits(),
            audit_end.slab_structure.flags.bits(),
            audit_end.kheap_structure.flags.bits(),
            audit_end.managed_structure.flags.bits(),
            audit_end.registry_live_records,
            audit_end.registry_kind_records,
            audit_end.registry_boot_records,
            audit_end.registry_physical_records,
            audit_end.registry_nodes_accounted,
            audit_end.registry_node_capacity,
            audit_end.registry_structure.scanned_live_records,
            audit_end.registry_structure.scanned_free_nodes,
            audit_end.slab_active_objects,
            audit_end.slab_live_records,
            audit_end.kheap_active_allocs,
            audit_end.kheap_live_records,
            audit_end.managed_active_objects,
            audit_end.managed_live_records
        );
    }

    log::info!(
        "[bench][alloc][cost] slab hot path ~= class routing + per-cpu cache + registry insert/remove + slab-node direct free: 8B={}ns 64B={}ns 80B/64align={}ns 1024B={}ns",
        slab8,
        slab64,
        slab80_align64,
        slab1024
    );
    log::info!(
        "[bench][alloc][cost] kheap path ~= vmem reserve/tag split + buddy page + map/unmap + registry: 4K={}ns 8K={}ns 64K={}ns",
        large4k,
        large8k,
        large64k
    );
    log_allocator_hotspot("end");
}

fn log_allocator_hotspot(stage: &str) {
    let hot = KERNEL_ALLOCATOR.hotspot_summary();
    log::info!(
        "[bench][alloc][hotspot][{}] phys_fail={}/1000 phys_split={}/1000alloc phys_merge={}/1000free phys_defer={}/1000free phys_reclaim={}/1000alloc phys_meta={}/1000 phys_corrupt={} slab_hit={}/1000 slab_miss={}/1000 refill={}/1000 flush={}/1000 fast_free={}/1000 fallback={} reg_chain={} reg_shard={} reg_load={}/1000 reg_underflow={} reg_corrupt={} kheap_fail={}/1000 kheap_realloc={}/1000 kheap_cache={}/1000 cached_pages={} pressure_rel={} vmem_largest={}pct vmem_free_segs={} managed_frag={}/1000 pressure={}",
        stage,
        hot.phys_alloc_failure_per_mille,
        hot.phys_split_per_alloc_mille,
        hot.phys_coalesce_per_free_mille,
        hot.phys_defer_per_free_mille,
        hot.phys_reclaim_per_alloc_mille,
        hot.phys_metadata_load_per_mille,
        hot.phys_chain_corruptions,
        hot.slab_cache_hit_per_mille,
        hot.slab_cache_miss_per_mille,
        hot.slab_refill_per_mille,
        hot.slab_flush_per_mille,
        hot.slab_fast_free_per_mille,
        hot.slab_fast_free_fallbacks,
        hot.registry_max_chain_len,
        hot.registry_max_shard_live_records,
        hot.registry_live_per_bucket_per_mille,
        hot.registry_underflows,
        hot.registry_chain_corruptions,
        hot.kheap_failure_per_mille,
        hot.kheap_realloc_per_mille,
        hot.kheap_cache_hit_per_mille,
        hot.kheap_cached_pages,
        hot.kheap_cache_pressure_releases,
        hot.kernel_vmem_largest_free_percent,
        hot.kernel_vmem_free_segments,
        hot.managed_fragmentation_per_mille,
        hot.pressure_level
    );
}

fn bench_per_mille(part: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    ((part as u128 * 1000) / total as u128).min(u64::MAX as u128) as u64
}

fn run_allocator_audit() {
    let iters = 4096usize;
    let t0 = hal::time::monotonic_ns();
    let mut audit_flags = 0u32;
    let mut registry_flags = 0u32;
    let mut phys_flags = 0u32;
    let mut slab_flags = 0u32;
    let mut kheap_flags = 0u32;
    let mut managed_flags = 0u32;
    let mut live = 0usize;
    for _ in 0..iters {
        let audit = KERNEL_ALLOCATOR.audit();
        audit_flags |= audit.flags.bits();
        registry_flags |= audit.registry_structure.flags.bits();
        phys_flags |= audit.phys_structure.flags.bits();
        slab_flags |= audit.slab_structure.flags.bits();
        kheap_flags |= audit.kheap_structure.flags.bits();
        managed_flags |= audit.managed_structure.flags.bits();
        live = live.saturating_add(audit.registry_live_records);
        core::hint::black_box(audit);
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][alloc][audit] {} snapshots avg {} ns/op audit_flags={} registry_flags={} phys_flags={} slab_flags={} kheap_flags={} managed_flags={} avg_live={}; source=layer stats snapshot + full registry/buddy/slab/kheap/managed structure scan + typed invariant checks",
        iters,
        dt / iters as u64,
        audit_flags,
        registry_flags,
        phys_flags,
        slab_flags,
        kheap_flags,
        managed_flags,
        live / iters
    );
}

fn run_allocator_counter_audit() {
    let iters = 4096usize;
    let t0 = hal::time::monotonic_ns();
    let mut audit_flags = 0u32;
    let mut scanned = 0usize;
    let mut live = 0usize;
    for _ in 0..iters {
        let audit = KERNEL_ALLOCATOR.audit_counters();
        audit_flags |= audit.flags.bits();
        scanned += audit.registry_structure_scanned as usize;
        live = live.saturating_add(audit.registry_live_records);
        core::hint::black_box(audit);
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][alloc][audit-counters] {} snapshots avg {} ns/op audit_flags={} scanned={} avg_live={}; source=layer stats snapshot + O(1) registry counters, no bucket scan",
        iters,
        dt / iters as u64,
        audit_flags,
        scanned,
        live / iters
    );
}

fn allocator_request(size: usize, align: usize, zeroing: Zeroing) -> MemoryRequest {
    MemoryRequest::new(MemoryDomain::Kernel, size, align)
        .with_zeroing(zeroing)
        .with_reclaim(ReclaimPolicy::NoReclaim)
}

#[derive(Clone, Copy)]
struct AllocFreeMeasurement {
    completed: usize,
    total_ns: u64,
    avg_ns: u64,
}

fn run_allocator_alloc_free_case(
    tag: &str,
    size: usize,
    align: usize,
    iters: usize,
    zeroing: Zeroing,
) -> u64 {
    let Some(measurement) = measure_allocator_alloc_free_case(tag, size, align, iters, zeroing)
    else {
        return 0;
    };
    log::info!(
        "[bench][alloc][{}] {} x alloc+free size={} align={} zero={:?}: total {} ns (avg {} ns/op)",
        tag,
        measurement.completed,
        size,
        align,
        zeroing,
        measurement.total_ns,
        measurement.avg_ns
    );
    measurement.avg_ns
}

fn measure_allocator_alloc_free_case(
    tag: &str,
    size: usize,
    align: usize,
    iters: usize,
    zeroing: Zeroing,
) -> Option<AllocFreeMeasurement> {
    let mut records = [None; ALLOC_BENCH_BATCH];
    let request = allocator_request(size, align, zeroing);
    let mut completed = 0usize;
    let t0 = hal::time::monotonic_ns();

    for _ in 0..iters {
        let mut allocated = 0usize;
        for slot in &mut records {
            match KERNEL_ALLOCATOR.allocate(request) {
                Ok(record) => {
                    core::hint::black_box(record.ptr);
                    *slot = Some(record);
                    allocated += 1;
                }
                Err(err) => {
                    log::error!(
                        "[bench][alloc][{}] allocate failed after {} objects: {:?}",
                        tag,
                        completed,
                        err
                    );
                    break;
                }
            }
        }
        for slot in records.iter_mut().take(allocated).rev() {
            if let Some(record) = slot.take() {
                if let Err(err) = KERNEL_ALLOCATOR.deallocate(record.ptr) {
                    log::error!(
                        "[bench][alloc][{}] deallocate failed ptr={:#x}: {:?}",
                        tag,
                        record.ptr,
                        err
                    );
                    return None;
                }
                completed += 1;
            }
        }
        if allocated != ALLOC_BENCH_BATCH {
            break;
        }
    }

    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    Some(AllocFreeMeasurement {
        completed,
        total_ns: dt,
        avg_ns: dt / completed.max(1) as u64,
    })
}

fn run_allocator_sampled_alloc_free_case(
    tag: &str,
    size: usize,
    align: usize,
    iters: usize,
    zeroing: Zeroing,
) {
    let mut samples = [0u64; ALLOC_BENCH_SAMPLES];
    let mut collected = 0usize;
    for slot in &mut samples {
        let Some(measurement) = measure_allocator_alloc_free_case(tag, size, align, iters, zeroing)
        else {
            break;
        };
        *slot = measurement.avg_ns;
        collected += 1;
    }
    if collected == 0 {
        return;
    }
    let samples = &mut samples[..collected];
    samples.sort_unstable();
    let p50 = samples[percentile_index(collected, 50)];
    let p95 = samples[percentile_index(collected, 95)];
    log::info!(
        "[bench][alloc][sample][{}] samples={} each={} objects size={} align={} zero={:?}: min={} p50={} p95={} max={} ns/op; source=repeated short alloc+free windows",
        tag,
        collected,
        iters * ALLOC_BENCH_BATCH,
        size,
        align,
        zeroing,
        samples[0],
        p50,
        p95,
        samples[collected - 1]
    );
}

fn percentile_index(count: usize, percentile: usize) -> usize {
    count
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1)
        .min(count.saturating_sub(1))
}

fn run_allocator_split_alloc_free_case(
    tag: &str,
    size: usize,
    align: usize,
    iters: usize,
    zeroing: Zeroing,
) {
    let mut records = [None; ALLOC_BENCH_BATCH];
    let request = allocator_request(size, align, zeroing);
    let mut completed = 0usize;
    let mut alloc_dt = 0u64;
    let mut free_dt = 0u64;

    for _ in 0..iters {
        let mut allocated = 0usize;
        let t0 = hal::time::monotonic_ns();
        for slot in &mut records {
            match KERNEL_ALLOCATOR.allocate(request) {
                Ok(record) => {
                    core::hint::black_box(record.ptr);
                    *slot = Some(record);
                    allocated += 1;
                }
                Err(err) => {
                    log::error!(
                        "[bench][alloc][split][{}] allocate failed after {} objects: {:?}",
                        tag,
                        completed,
                        err
                    );
                    break;
                }
            }
        }
        alloc_dt = alloc_dt.saturating_add(hal::time::monotonic_ns().saturating_sub(t0));

        let t0 = hal::time::monotonic_ns();
        for slot in records.iter_mut().take(allocated).rev() {
            if let Some(record) = slot.take() {
                if let Err(err) = KERNEL_ALLOCATOR.deallocate(record.ptr) {
                    log::error!(
                        "[bench][alloc][split][{}] deallocate failed ptr={:#x}: {:?}",
                        tag,
                        record.ptr,
                        err
                    );
                    return;
                }
                completed += 1;
            }
        }
        free_dt = free_dt.saturating_add(hal::time::monotonic_ns().saturating_sub(t0));
        if allocated != ALLOC_BENCH_BATCH {
            break;
        }
    }

    let ops = completed.max(1) as u64;
    log::info!(
        "[bench][alloc][split][{}] {} objects size={} align={} zero={:?}: alloc {} ns/op free {} ns/op; source=route+backend+registry-insert vs registry-remove+backend-release",
        tag,
        completed,
        size,
        align,
        zeroing,
        alloc_dt / ops,
        free_dt / ops
    );
}

fn run_allocator_kheap_full_cache_reuse() {
    const COUNT: usize = 160;
    let mut records = [None; COUNT];
    let request = allocator_request(PAGE_SIZE, PAGE_SIZE, Zeroing::Uninitialized);

    for slot in &mut records {
        match KERNEL_ALLOCATOR.allocate(request) {
            Ok(record) => {
                *slot = Some(record);
            }
            Err(err) => {
                log::error!(
                    "[bench][alloc][kheap-cache-full] allocate failed during fill: {:?}",
                    err
                );
                for slot in &mut records {
                    if let Some(record) = slot.take() {
                        let _ = KERNEL_ALLOCATOR.deallocate(record.ptr);
                    }
                }
                return;
            }
        }
    }

    let sentinel = records[COUNT - 1].take().expect("sentinel exists");
    for slot in records.iter_mut().take(COUNT - 1) {
        if let Some(record) = slot.take() {
            if let Err(err) = KERNEL_ALLOCATOR.deallocate(record.ptr) {
                log::error!(
                    "[bench][alloc][kheap-cache-full] deallocate fill ptr={:#x} failed: {:?}",
                    record.ptr,
                    err
                );
                let _ = KERNEL_ALLOCATOR.deallocate(sentinel.ptr);
                return;
            }
        }
    }

    let before = KERNEL_ALLOCATOR.layer_stats().kheap;
    let t0 = hal::time::monotonic_ns();
    if let Err(err) = KERNEL_ALLOCATOR.deallocate(sentinel.ptr) {
        log::error!(
            "[bench][alloc][kheap-cache-full] deallocate sentinel ptr={:#x} failed: {:?}",
            sentinel.ptr,
            err
        );
        return;
    }
    let free_dt = hal::time::monotonic_ns().saturating_sub(t0);

    let t0 = hal::time::monotonic_ns();
    let reused = match KERNEL_ALLOCATOR.allocate(request) {
        Ok(record) => record,
        Err(err) => {
            log::error!(
                "[bench][alloc][kheap-cache-full] allocate reused sentinel failed: {:?}",
                err
            );
            return;
        }
    };
    let alloc_dt = hal::time::monotonic_ns().saturating_sub(t0);
    let after = KERNEL_ALLOCATOR.layer_stats().kheap;
    let hot_reuse = (reused.ptr == sentinel.ptr) as usize;
    log::info!(
        "[bench][alloc][kheap-cache-full] sentinel hot={} free {} ns alloc {} ns full_release_delta={} cache_hit_delta={}; source=full-cache ring replace-oldest + LIFO reuse",
        hot_reuse,
        free_dt,
        alloc_dt,
        after
            .cache_full_releases
            .saturating_sub(before.cache_full_releases),
        after.cache_hits.saturating_sub(before.cache_hits)
    );
    let _ = KERNEL_ALLOCATOR.deallocate(reused.ptr);
}

fn run_allocator_reclaim_api() {
    const KHEAP_COUNT: usize = 8;
    const SLAB_COUNT: usize = 16;
    let mut kheap_records = [None; KHEAP_COUNT];
    let mut slab_records = [None; SLAB_COUNT];

    for slot in &mut kheap_records {
        match KERNEL_ALLOCATOR.allocate(allocator_request(
            PAGE_SIZE,
            PAGE_SIZE,
            Zeroing::Uninitialized,
        )) {
            Ok(record) => *slot = Some(record),
            Err(err) => {
                log::error!(
                    "[bench][alloc][reclaim] kheap setup alloc failed: {:?}",
                    err
                );
                return;
            }
        }
    }
    for slot in &mut slab_records {
        match KERNEL_ALLOCATOR.allocate(allocator_request(64, 8, Zeroing::Uninitialized)) {
            Ok(record) => *slot = Some(record),
            Err(err) => {
                log::error!("[bench][alloc][reclaim] slab setup alloc failed: {:?}", err);
                for slot in &mut kheap_records {
                    if let Some(record) = slot.take() {
                        let _ = KERNEL_ALLOCATOR.deallocate(record.ptr);
                    }
                }
                return;
            }
        }
    }

    for slot in &mut kheap_records {
        if let Some(record) = slot.take() {
            let _ = KERNEL_ALLOCATOR.deallocate(record.ptr);
        }
    }
    for slot in &mut slab_records {
        if let Some(record) = slot.take() {
            let _ = KERNEL_ALLOCATOR.deallocate(record.ptr);
        }
    }

    let before = KERNEL_ALLOCATOR.layer_stats();
    let t0 = hal::time::monotonic_ns();
    let reclaim = match KERNEL_ALLOCATOR
        .reclaim(AllocatorReclaimRequest::caches().without_physical_deferred_reclaim())
    {
        Ok(reclaim) => reclaim,
        Err(err) => {
            log::error!("[bench][alloc][reclaim] api failed: {:?}", err);
            return;
        }
    };
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let after = KERNEL_ALLOCATOR.layer_stats();
    log::info!(
        "[bench][alloc][reclaim] one-shot {} ns kheap_ranges={} kheap_pages={} slab_flush={} slab_slabs={} bytes={} cached_pages {}->{} slab_flush_delta={}; source=public pressure/cache maintenance API",
        dt,
        reclaim.kheap.released_ranges,
        reclaim.kheap.released_pages,
        reclaim.slab.flushed_cached_objects,
        reclaim.slab.reclaimed_slabs,
        reclaim.reclaimed_bytes(),
        before.kheap.cached_pages,
        after.kheap.cached_pages,
        after
            .slab
            .cache_flushes
            .saturating_sub(before.slab.cache_flushes)
    );
}

fn run_allocator_registry_lookup() {
    let request = allocator_request(64, 8, Zeroing::Uninitialized);
    let record = match KERNEL_ALLOCATOR.allocate(request) {
        Ok(record) => record,
        Err(err) => {
            log::error!("[bench][alloc][registry] setup alloc failed: {:?}", err);
            return;
        }
    };

    let iters = 16 * 1024usize;
    let t0 = hal::time::monotonic_ns();
    for _ in 0..iters {
        let found = KERNEL_ALLOCATOR.query_tracked_allocation(record.ptr).ok();
        core::hint::black_box(found);
    }
    let hit_dt = hal::time::monotonic_ns().saturating_sub(t0);

    let miss_ptr = usize::MAX - PAGE_SIZE + 1;
    let t0 = hal::time::monotonic_ns();
    for _ in 0..iters {
        let found = KERNEL_ALLOCATOR.query_tracked_allocation(miss_ptr).ok();
        core::hint::black_box(found);
    }
    let miss_dt = hal::time::monotonic_ns().saturating_sub(t0);

    let _ = KERNEL_ALLOCATOR.deallocate(record.ptr);
    log::info!(
        "[bench][alloc][registry] lookup hit avg {} ns/op, miss avg {} ns/op; source=bucket lock + chain scan",
        hit_dt / iters as u64,
        miss_dt / iters as u64
    );
}

fn run_allocator_sampled_registry_lookup() {
    let request = allocator_request(64, 8, Zeroing::Uninitialized);
    let record = match KERNEL_ALLOCATOR.allocate(request) {
        Ok(record) => record,
        Err(err) => {
            log::error!(
                "[bench][alloc][registry-sample] setup alloc failed: {:?}",
                err
            );
            return;
        }
    };

    let iters = 4096usize;
    let miss_ptr = usize::MAX - PAGE_SIZE + 1;
    let mut hit_samples = [0u64; ALLOC_BENCH_SAMPLES];
    let mut miss_samples = [0u64; ALLOC_BENCH_SAMPLES];
    for idx in 0..ALLOC_BENCH_SAMPLES {
        let t0 = hal::time::monotonic_ns();
        for _ in 0..iters {
            let found = KERNEL_ALLOCATOR.query_tracked_allocation(record.ptr).ok();
            core::hint::black_box(found);
        }
        hit_samples[idx] = hal::time::monotonic_ns().saturating_sub(t0) / iters as u64;

        let t0 = hal::time::monotonic_ns();
        for _ in 0..iters {
            let found = KERNEL_ALLOCATOR.query_tracked_allocation(miss_ptr).ok();
            core::hint::black_box(found);
        }
        miss_samples[idx] = hal::time::monotonic_ns().saturating_sub(t0) / iters as u64;
    }

    let _ = KERNEL_ALLOCATOR.deallocate(record.ptr);
    hit_samples.sort_unstable();
    miss_samples.sort_unstable();
    let p50_idx = percentile_index(ALLOC_BENCH_SAMPLES, 50);
    let p95_idx = percentile_index(ALLOC_BENCH_SAMPLES, 95);
    log::info!(
        "[bench][alloc][sample][registry] samples={} each={} lookups: hit min={} p50={} p95={} max={} miss min={} p50={} p95={} max={} ns/op; source=repeated bucket lock + chain scan windows",
        ALLOC_BENCH_SAMPLES,
        iters,
        hit_samples[0],
        hit_samples[p50_idx],
        hit_samples[p95_idx],
        hit_samples[ALLOC_BENCH_SAMPLES - 1],
        miss_samples[0],
        miss_samples[p50_idx],
        miss_samples[p95_idx],
        miss_samples[ALLOC_BENCH_SAMPLES - 1]
    );
}

fn run_allocator_in_place_query() {
    let request = allocator_request(64, 8, Zeroing::Uninitialized);
    let record = match KERNEL_ALLOCATOR.allocate(request) {
        Ok(record) => record,
        Err(err) => {
            log::error!(
                "[bench][alloc][in-place-query] setup alloc failed: {:?}",
                err
            );
            return;
        }
    };

    let same_class =
        MemoryRequest::new(MemoryDomain::Kernel, 63, 8).with_reclaim(ReclaimPolicy::NoReclaim);
    let moving =
        MemoryRequest::new(MemoryDomain::Kernel, 4096, 8).with_reclaim(ReclaimPolicy::NoReclaim);
    let iters = 16 * 1024usize;

    let t0 = hal::time::monotonic_ns();
    for _ in 0..iters {
        let can = KERNEL_ALLOCATOR
            .can_reallocate_in_place(record.ptr, same_class)
            .ok();
        core::hint::black_box(can);
    }
    let same_dt = hal::time::monotonic_ns().saturating_sub(t0);

    let t0 = hal::time::monotonic_ns();
    for _ in 0..iters {
        let can = KERNEL_ALLOCATOR
            .can_reallocate_in_place(record.ptr, moving)
            .ok();
        core::hint::black_box(can);
    }
    let moving_dt = hal::time::monotonic_ns().saturating_sub(t0);

    let _ = KERNEL_ALLOCATOR.deallocate(record.ptr);
    log::info!(
        "[bench][alloc][in-place-query] same-class avg {} ns/op, moving-needed avg {} ns/op; source=registry lookup + slab class/kheap order predicate",
        same_dt / iters as u64,
        moving_dt / iters as u64
    );
}

fn run_allocator_realloc_same_class() {
    let old_layout = Layout::from_size_align(64, 8).expect("valid realloc old layout");
    let new_layout = Layout::from_size_align(63, 8).expect("valid realloc new layout");
    let mut ptrs = [core::ptr::null_mut(); ALLOC_BENCH_BATCH];
    let mut completed = 0usize;
    let iters = 256usize;
    let mut total_dt = 0u64;

    for _ in 0..iters {
        let mut allocated = 0usize;
        for ptr in &mut ptrs {
            let p = unsafe { GlobalAlloc::alloc(&KERNEL_ALLOCATOR, old_layout) };
            if p.is_null() {
                break;
            }
            *ptr = p;
            allocated += 1;
        }
        let t0 = hal::time::monotonic_ns();
        for ptr in ptrs.iter_mut().take(allocated) {
            let new_ptr = unsafe { GlobalAlloc::realloc(&KERNEL_ALLOCATOR, *ptr, old_layout, 63) };
            if new_ptr.is_null() {
                log::error!("[bench][alloc][realloc-same] realloc returned null");
                break;
            }
            core::hint::black_box(new_ptr);
            *ptr = new_ptr;
            completed += 1;
        }
        total_dt = total_dt.saturating_add(hal::time::monotonic_ns().saturating_sub(t0));
        for ptr in ptrs.iter_mut().take(allocated) {
            if !(*ptr).is_null() {
                unsafe { GlobalAlloc::dealloc(&KERNEL_ALLOCATOR, *ptr, new_layout) };
                *ptr = core::ptr::null_mut();
            }
        }
        if allocated != ALLOC_BENCH_BATCH {
            break;
        }
    }

    log::info!(
        "[bench][alloc][realloc-same] {} x 64B->63B avg {} ns/op; source=single registry shard update + in-place size update",
        completed,
        total_dt / completed.max(1) as u64
    );
}

fn run_allocator_typed_realloc_same_class() {
    let old_request = allocator_request(64, 8, Zeroing::Uninitialized);
    let new_request = allocator_request(63, 8, Zeroing::Uninitialized);
    let mut records = [None; ALLOC_BENCH_BATCH];
    let mut completed = 0usize;
    let iters = 256usize;
    let mut total_dt = 0u64;

    for _ in 0..iters {
        let mut allocated = 0usize;
        for slot in &mut records {
            match KERNEL_ALLOCATOR.allocate(old_request) {
                Ok(record) => {
                    *slot = Some(record);
                    allocated += 1;
                }
                Err(err) => {
                    log::error!(
                        "[bench][alloc][typed-realloc-same] allocate failed after {} objects: {:?}",
                        completed,
                        err
                    );
                    break;
                }
            }
        }

        let t0 = hal::time::monotonic_ns();
        for slot in records.iter_mut().take(allocated) {
            let Some(record) = *slot else {
                continue;
            };
            match KERNEL_ALLOCATOR.reallocate(record.ptr, new_request) {
                Ok(resized) => {
                    core::hint::black_box(resized.ptr);
                    *slot = Some(resized);
                    completed += 1;
                }
                Err(err) => {
                    log::error!(
                        "[bench][alloc][typed-realloc-same] reallocate ptr={:#x} failed: {:?}",
                        record.ptr,
                        err
                    );
                    break;
                }
            }
        }
        total_dt = total_dt.saturating_add(hal::time::monotonic_ns().saturating_sub(t0));

        for slot in records.iter_mut().take(allocated) {
            if let Some(record) = slot.take() {
                if let Err(err) = KERNEL_ALLOCATOR.deallocate(record.ptr) {
                    log::error!(
                        "[bench][alloc][typed-realloc-same] deallocate ptr={:#x} failed: {:?}",
                        record.ptr,
                        err
                    );
                    return;
                }
            }
        }
        if allocated != ALLOC_BENCH_BATCH {
            break;
        }
    }

    log::info!(
        "[bench][alloc][typed-realloc-same] {} x 64B->63B avg {} ns/op; source=typed MemoryRequest validation + single registry shard update",
        completed,
        total_dt / completed.max(1) as u64
    );
}

fn run_allocator_realloc_grow() {
    let old_layout = Layout::from_size_align(64, 8).expect("valid realloc old layout");
    let new_layout = Layout::from_size_align(4096, 8).expect("valid realloc new layout");
    let mut ptrs = [core::ptr::null_mut(); ALLOC_BENCH_BATCH];
    let mut completed = 0usize;
    let iters = 64usize;
    let mut total_dt = 0u64;

    for _ in 0..iters {
        let mut allocated = 0usize;
        for ptr in &mut ptrs {
            let p = unsafe { GlobalAlloc::alloc(&KERNEL_ALLOCATOR, old_layout) };
            if p.is_null() {
                break;
            }
            *ptr = p;
            allocated += 1;
        }
        let t0 = hal::time::monotonic_ns();
        for ptr in ptrs.iter_mut().take(allocated) {
            let new_ptr =
                unsafe { GlobalAlloc::realloc(&KERNEL_ALLOCATOR, *ptr, old_layout, 4096) };
            if new_ptr.is_null() {
                log::error!("[bench][alloc][realloc-grow] realloc returned null");
                break;
            }
            core::hint::black_box(new_ptr);
            *ptr = new_ptr;
            completed += 1;
        }
        total_dt = total_dt.saturating_add(hal::time::monotonic_ns().saturating_sub(t0));
        for ptr in ptrs.iter_mut().take(allocated) {
            if !(*ptr).is_null() {
                unsafe { GlobalAlloc::dealloc(&KERNEL_ALLOCATOR, *ptr, new_layout) };
                *ptr = core::ptr::null_mut();
            }
        }
        if allocated != ALLOC_BENCH_BATCH {
            break;
        }
    }

    log::info!(
        "[bench][alloc][realloc-grow] {} x 64B->4K avg {} ns/op; source=new kheap alloc + copy + registry-cookie old slab release",
        completed,
        total_dt / completed.max(1) as u64
    );
}

fn run_allocator_diagnostic() {
    let mut buf = [0u8; 2048];
    let iters = 512usize;
    let t0 = hal::time::monotonic_ns();
    let mut total_len = 0usize;
    for _ in 0..iters {
        let len = KERNEL_ALLOCATOR.format_diagnostic(&mut buf);
        total_len = total_len.saturating_add(len);
        core::hint::black_box(len);
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][alloc][diagnostic] {} snapshots avg {} ns/op avg_len={}; source=stats snapshot + full registry/buddy/slab/kheap/managed audit scan + no_alloc formatting",
        iters,
        dt / iters as u64,
        total_len / iters
    );
}

fn run_allocator_counter_diagnostic() {
    let mut buf = [0u8; 2048];
    let iters = 512usize;
    let t0 = hal::time::monotonic_ns();
    let mut total_len = 0usize;
    for _ in 0..iters {
        let len = KERNEL_ALLOCATOR
            .format_diagnostic_with_scope(&mut buf, AllocatorAuditScope::CountersOnly);
        total_len = total_len.saturating_add(len);
        core::hint::black_box(len);
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][alloc][diagnostic-counters] {} snapshots avg {} ns/op avg_len={}; source=stats snapshot + O(1) registry counters + no_alloc formatting",
        iters,
        dt / iters as u64,
        total_len / iters
    );
}

fn run_allocator_physical() {
    let mut pages = [None; ALLOC_BENCH_BATCH];
    let request = PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE);
    let iters = 64usize;
    let mut completed = 0usize;
    let before = KERNEL_ALLOCATOR.buddy_stats();
    let t0 = hal::time::monotonic_ns();

    for _ in 0..iters {
        let mut allocated = 0usize;
        for slot in &mut pages {
            match KERNEL_ALLOCATOR.allocate_physical(request) {
                Ok(page) => {
                    core::hint::black_box(page.paddr);
                    *slot = Some(page);
                    allocated += 1;
                }
                Err(err) => {
                    log::error!(
                        "[bench][alloc][physical] allocate failed after {} pages: {:?}",
                        completed,
                        err
                    );
                    break;
                }
            }
        }
        for slot in pages.iter_mut().take(allocated).rev() {
            if let Some(page) = slot.take() {
                if let Err(err) = KERNEL_ALLOCATOR.try_free_physical(page) {
                    log::error!(
                        "[bench][alloc][physical] free failed paddr={:#x}: {:?}",
                        page.paddr,
                        err
                    );
                    return;
                }
                completed += 1;
            }
        }
        if allocated != ALLOC_BENCH_BATCH {
            break;
        }
    }

    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let after = KERNEL_ALLOCATOR.buddy_stats();
    let alloc_delta = after.alloc_requests.saturating_sub(before.alloc_requests);
    let free_delta = after.free_requests.saturating_sub(before.free_requests);
    let split_delta = after.split_count.saturating_sub(before.split_count);
    let coalesce_delta = after.coalesce_count.saturating_sub(before.coalesce_count);
    let defer_delta = after
        .deferred_coalesce_count
        .saturating_sub(before.deferred_coalesce_count);
    let reclaim_delta = after
        .deferred_reclaim_count
        .saturating_sub(before.deferred_reclaim_count);
    let failure_delta = after.alloc_failures.saturating_sub(before.alloc_failures);
    log::info!(
        "[bench][alloc][physical] {} x page alloc+free avg {} ns/op split={} merge={} defer={} reclaim={} fail={} split_per_alloc={}/1000 merge_per_free={}/1000 defer_per_free={}/1000 reclaim_per_alloc={}/1000; source=buddy lock + split/coalesce/defer + registry tracking",
        completed,
        dt / completed.max(1) as u64,
        split_delta,
        coalesce_delta,
        defer_delta,
        reclaim_delta,
        failure_delta,
        bench_per_mille(split_delta, alloc_delta),
        bench_per_mille(coalesce_delta, free_delta),
        bench_per_mille(defer_delta, free_delta),
        bench_per_mille(reclaim_delta, alloc_delta)
    );
}

fn run_allocator_physical_exact_reclaim() {
    const COUNT: usize = 16;
    let mut pages = [None; COUNT];
    for slot in &mut pages {
        match KERNEL_ALLOCATOR.allocate_physical(PhysicalAllocRequest::new(PAGE_SIZE, PAGE_SIZE)) {
            Ok(page) => *slot = Some(page),
            Err(err) => {
                log::error!(
                    "[bench][alloc][physical-exact-reclaim] source page allocate failed: {:?}",
                    err
                );
                return;
            }
        }
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
    let Some(exact_base) = exact_base else {
        log::error!("[bench][alloc][physical-exact-reclaim] no order-1 buddy pair found");
        for slot in &mut pages {
            if let Some(page) = slot.take() {
                let _ = KERNEL_ALLOCATOR.try_free_physical(page);
            }
        }
        return;
    };

    for slot in pages.iter_mut().rev() {
        if let Some(page) = slot.take() {
            if let Err(err) = KERNEL_ALLOCATOR.try_free_physical(page) {
                log::error!(
                    "[bench][alloc][physical-exact-reclaim] source free paddr={:#x} failed: {:?}",
                    page.paddr,
                    err
                );
                return;
            }
        }
    }

    let before = KERNEL_ALLOCATOR.buddy_stats();
    let request = PhysicalAllocRequest::new(PAGE_SIZE * 2, PAGE_SIZE * 2)
        .with_placement(MemoryPlacement::ExactPhys(exact_base));
    let t0 = hal::time::monotonic_ns();
    let allocation = match KERNEL_ALLOCATOR.allocate_physical(request) {
        Ok(allocation) => allocation,
        Err(err) => {
            log::error!(
                "[bench][alloc][physical-exact-reclaim] exact order-1 allocate failed base={:#x}: {:?}",
                exact_base,
                err
            );
            return;
        }
    };
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let after = KERNEL_ALLOCATOR.buddy_stats();
    log::info!(
        "[bench][alloc][physical-exact-reclaim] exact order-1 base={:#x} alloc {} ns reclaim_delta={} merge_delta={} split_delta={}; source=deferred order0 reclaim before ExactPhys",
        exact_base,
        dt,
        after
            .deferred_reclaim_count
            .saturating_sub(before.deferred_reclaim_count),
        after.coalesce_count.saturating_sub(before.coalesce_count),
        after.split_count.saturating_sub(before.split_count)
    );

    let _ = KERNEL_ALLOCATOR.try_free_physical(allocation);
}

#[cfg(feature = "bench")]
fn make_ram_device(name: &str, image: &'static [u8]) -> Arc<BlockDevice> {
    const LBS: u32 = 512;
    let block_count = (image.len() as u64) / LBS as u64;
    let mut backing: Vec<u8> = vec![0; image.len()];
    backing.copy_from_slice(image);
    let io = Arc::new(RamBlockIo::new(backing));
    let geom = BlockGeometry::new(
        NonZeroU32::new(LBS).unwrap(),
        NonZeroU32::new(LBS).unwrap(),
        Some(block_count),
    )
    .expect("ram geometry");
    Arc::new(BlockDevice::new(
        BlockDeviceInit {
            name,
            subsystem: "ramdisk",
            class: BlockClass::Whole,
            geometry: geom,
            limits: BlockLimits::unrestricted(),
            attributes: Default::default(),
            features: BlockFeatures::FLUSH,
        },
        io,
        None,
    ))
}

#[cfg(feature = "bench")]
fn mount_fat(tag: &str, dev: Arc<BlockDevice>) -> Option<Arc<Superblock>> {
    let backend = match SyncBlockBackend::new(Arc::clone(&dev)) {
        Ok(backend) => Arc::new(backend),
        Err(e) => {
            log::error!("[bench][{}] backend unavailable: {:?}", tag, e);
            return None;
        }
    };
    let driver = fatfs::FatFsDriver::new();
    driver.bind_backend(backend);
    match driver.mount(None, "") {
        Ok(sb) => {
            log::info!("[bench][{}] mount OK (fat32)", tag);
            Some(sb)
        }
        Err(e) => {
            log::error!("[bench][{}] mount failed: {:?}", tag, e);
            None
        }
    }
}

#[cfg(feature = "bench")]
fn mount_ext(tag: &str, dev: Arc<BlockDevice>) -> Option<Arc<Superblock>> {
    let backend = match SyncBlockBackend::new(Arc::clone(&dev)) {
        Ok(backend) => Arc::new(backend),
        Err(e) => {
            log::error!("[bench][{}] backend unavailable: {:?}", tag, e);
            return None;
        }
    };
    let driver = extfs::ExtFsDriver::new();
    driver.bind_backend(backend);
    match driver.mount(None, "") {
        Ok(sb) => {
            log::info!("[bench][{}] mount OK (ext4)", tag);
            Some(sb)
        }
        Err(e) => {
            log::error!("[bench][{}] mount failed: {:?}", tag, e);
            None
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// L0: 纯内存拷贝
// ═══════════════════════════════════════════════════════════════════════

#[cfg(any(feature = "bench", feature = "block-bench"))]
fn run_memcpy_baseline() {
    let size = 4 * 1024 * 1024usize;
    let src: Vec<u8> = vec![0xAA; size];
    let mut dst: Vec<u8> = vec![0; size];
    let t0 = hal::time::monotonic_ns();
    dst.copy_from_slice(&src);
    core::hint::black_box(&dst);
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let mibps = (size as u64) * 1_000_000_000 / dt.max(1) / (1024 * 1024);
    log::info!(
        "[bench][L0-hot] memcpy 4 MiB (cache hot): {} ns ({} MiB/s)",
        dt,
        mibps
    );
}

/// L0-cold: 从 64 MiB 冷数据源拷贝 4 MiB，强制 cache miss。
/// 如果结果 ≈ L1，说明瓶颈是内存访问而非软件。
#[cfg(any(feature = "bench", feature = "block-bench"))]
fn run_memcpy_cold() {
    let cold_size = 64 * 1024 * 1024usize;
    let cold_src: Vec<u8> = vec![0xBB; cold_size];
    let mut dst: Vec<u8> = vec![0; 4 * 1024 * 1024];
    let off = 32 * 1024 * 1024;
    let src_slice = &cold_src[off..off + 4 * 1024 * 1024];
    let t0 = hal::time::monotonic_ns();
    dst.copy_from_slice(src_slice);
    core::hint::black_box(&dst);
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let mibps = (dst.len() as u64) * 1_000_000_000 / dt.max(1) / (1024 * 1024);
    log::info!(
        "[bench][L0-cold] memcpy 4 MiB (from 64 MiB cold): {} ns ({} MiB/s)  <-- 真实内存带宽",
        dt,
        mibps
    );
}

/// 测量纯软件开销：读 1 个扇区（512B），数据拷贝可忽略，
/// 剩下的全是 lock/unlock + validation + vtable + atomic ops。
#[cfg(feature = "bench")]
fn run_software_overhead_only() {
    let image: &[u8] = &[0u8; 4096];
    let dev = {
        const LBS: u32 = 512;
        let block_count = (image.len() as u64) / LBS as u64;
        let mut backing: Vec<u8> = vec![0; image.len()];
        backing.copy_from_slice(image);
        let io = Arc::new(RamBlockIo::new(backing));
        let geom = BlockGeometry::new(
            NonZeroU32::new(LBS).unwrap(),
            NonZeroU32::new(LBS).unwrap(),
            Some(block_count),
        )
        .expect("geom");
        Arc::new(BlockDevice::new(
            BlockDeviceInit {
                name: "ramd-tiny",
                subsystem: "ramdisk",
                class: BlockClass::Whole,
                geometry: geom,
                limits: BlockLimits::unrestricted(),
                attributes: Default::default(),
                features: BlockFeatures::FLUSH,
            },
            io,
            None,
        ))
    };
    let backend = match SyncBlockBackend::new(Arc::clone(&dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][SW-overhead] backend unavailable: {:?}", e);
            return;
        }
    };
    let mut buf = [0u8; 512];
    let count = 1000u64;
    let t0 = hal::time::monotonic_ns();
    for _ in 0..count {
        let _ = backend.read(0, 1, &mut buf);
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let avg = dt / count;
    log::info!(
        "[bench][SW-overhead] 1000x read 512B: total {} ns (avg {} ns/op)  <-- 纯软件开销/op",
        dt,
        avg
    );
}

// ═══════════════════════════════════════════════════════════════════════
// L1: 裸块设备顺序读写
// ═══════════════════════════════════════════════════════════════════════

#[cfg(any(feature = "bench", feature = "block-bench"))]
fn block_device_has_bytes(dev: &BlockDevice, bytes: usize) -> bool {
    dev.geometry()
        .capacity_bytes()
        .is_some_and(|capacity| capacity >= bytes as u64)
}

#[cfg(feature = "block-bench")]
fn run_block_small_read(tag: &str, dev: &Arc<BlockDevice>) {
    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][{}][SW-block] backend unavailable: {:?}", tag, e);
            return;
        }
    };
    let lbs = dev.geometry().logical_block_size().get() as usize;
    let mut buf = vec![0u8; lbs];
    let count = 1000u64;
    let t0 = hal::time::monotonic_ns();
    let mut errors = 0u64;
    for _ in 0..count {
        if backend.read(0, 1, &mut buf).is_err() {
            errors += 1;
        }
        core::hint::black_box(&buf);
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][{}][SW-block] {}x read {}B from block device: total {} ns avg {} ns/op err={}",
        tag,
        count,
        lbs,
        dt,
        dt / count,
        errors
    );
}

#[cfg(feature = "block-bench")]
fn run_block_rw_flush_validation(tag: &str, dev: &Arc<BlockDevice>) {
    let read_only = match dev.control(BlockControlRequest::GetReadOnly) {
        Ok(BlockControlResponse::Bool(read_only)) => read_only,
        Ok(other) => {
            log::error!(
                "[bench][{}][RW-flush] unexpected read-only response: {:?}",
                tag,
                other
            );
            return;
        }
        Err(err) => {
            log::error!(
                "[bench][{}][RW-flush] failed to query read-only state: {:?}",
                tag,
                err
            );
            return;
        }
    };
    if read_only {
        log::warning!("[bench][{}][RW-flush] skipped read-only block device", tag);
        return;
    }

    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][{}][RW-flush] backend unavailable: {:?}", tag, e);
            return;
        }
    };
    let lbs = dev.geometry().logical_block_size().get() as usize;
    if !block_device_has_bytes(dev, lbs) {
        log::error!(
            "[bench][{}][RW-flush] device too small for one logical block",
            tag
        );
        return;
    }

    let mut before = vec![0u8; lbs];
    let mut after = vec![0u8; lbs];
    if let Err(err) = backend.read(0, 1, &mut before) {
        log::error!("[bench][{}][RW-flush] initial read failed: {:?}", tag, err);
        return;
    }

    // 用原始内容原样写回同一块，既覆盖 virtio write 路径，又不改变镜像语义。
    if let Err(err) = backend.write(0, 1, &before) {
        log::error!("[bench][{}][RW-flush] write-back failed: {:?}", tag, err);
        return;
    }

    match dev.control(BlockControlRequest::Flush) {
        Ok(BlockControlResponse::Done) => {}
        Ok(other) => {
            log::error!(
                "[bench][{}][RW-flush] unexpected flush response: {:?}",
                tag,
                other
            );
            return;
        }
        Err(err) => {
            log::error!("[bench][{}][RW-flush] flush failed: {:?}", tag, err);
            return;
        }
    }

    if let Err(err) = backend.read(0, 1, &mut after) {
        log::error!("[bench][{}][RW-flush] readback failed: {:?}", tag, err);
        return;
    }
    if before != after {
        log::error!(
            "[bench][{}][RW-flush] readback mismatch after write-back",
            tag
        );
        return;
    }
    log::info!(
        "[bench][{}][RW-flush] read/write/flush/readback ok bytes={}",
        tag,
        lbs
    );
}

#[cfg(feature = "block-bench")]
fn run_block_range_ops_validation(tag: &str, dev: &Arc<BlockDevice>) {
    let features = dev.features();
    if !features.contains(BlockFeatures::DISCARD) && !features.contains(BlockFeatures::WRITE_ZEROES)
    {
        log::warning!(
            "[bench][{}][range-op] skipped: discard/write-zeroes unsupported",
            tag
        );
        return;
    }

    match dev.control(BlockControlRequest::GetReadOnly) {
        Ok(BlockControlResponse::Bool(true)) => {
            log::warning!("[bench][{}][range-op] skipped read-only block device", tag);
            return;
        }
        Ok(BlockControlResponse::Bool(false)) => {}
        Ok(other) => {
            log::error!(
                "[bench][{}][range-op] unexpected read-only response: {:?}",
                tag,
                other
            );
            return;
        }
        Err(err) => {
            log::error!(
                "[bench][{}][range-op] failed to query read-only state: {:?}",
                tag,
                err
            );
            return;
        }
    }

    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][{}][range-op] backend unavailable: {:?}", tag, e);
            return;
        }
    };
    let Some(lba) = select_blank_range_test_lba(tag, dev, &backend, features) else {
        return;
    };
    let lbs = dev.geometry().logical_block_size().get() as usize;
    let mut original = vec![0u8; lbs];
    if let Err(err) = backend.read(lba, 1, &mut original) {
        log::error!(
            "[bench][{}][range-op] failed to read test block lba={}: {:?}",
            tag,
            lba,
            err
        );
        return;
    }
    let pattern = range_validation_pattern(lbs);

    if features.contains(BlockFeatures::WRITE_ZEROES) {
        if let Err(err) = backend.write(lba, 1, &pattern) {
            log::error!(
                "[bench][{}][range-op] prepare write-zeroes pattern failed lba={}: {:?}",
                tag,
                lba,
                err
            );
            return;
        }
        if let Err(err) = submit_range_op(dev, BioOp::WriteZeroes, lba) {
            restore_range_probe_block(tag, dev, &backend, lba, &original);
            log::error!(
                "[bench][{}][range-op] write-zeroes failed lba={}: {:?}",
                tag,
                lba,
                err
            );
            return;
        }
        let mut after = vec![0u8; lbs];
        if let Err(err) = backend.read(lba, 1, &mut after) {
            restore_range_probe_block(tag, dev, &backend, lba, &original);
            log::error!(
                "[bench][{}][range-op] write-zeroes readback failed lba={}: {:?}",
                tag,
                lba,
                err
            );
            return;
        }
        if !after.iter().all(|byte| *byte == 0) {
            restore_range_probe_block(tag, dev, &backend, lba, &original);
            log::error!(
                "[bench][{}][range-op] write-zeroes readback is not zero lba={}",
                tag,
                lba
            );
            return;
        }
        log::info!(
            "[bench][{}][range-op] write-zeroes ok lba={} bytes={}",
            tag,
            lba,
            lbs
        );
    }

    if features.contains(BlockFeatures::DISCARD) {
        if let Err(err) = backend.write(lba, 1, &pattern) {
            restore_range_probe_block(tag, dev, &backend, lba, &original);
            log::error!(
                "[bench][{}][range-op] prepare discard pattern failed lba={}: {:?}",
                tag,
                lba,
                err
            );
            return;
        }
        if let Err(err) = submit_range_op(dev, BioOp::Discard, lba) {
            restore_range_probe_block(tag, dev, &backend, lba, &original);
            log::error!(
                "[bench][{}][range-op] discard failed lba={}: {:?}",
                tag,
                lba,
                err
            );
            return;
        }
        restore_range_probe_block(tag, dev, &backend, lba, &original);
        log::info!("[bench][{}][range-op] discard ok lba={}", tag, lba);
    } else {
        restore_range_probe_block(tag, dev, &backend, lba, &original);
    }
}

#[cfg(feature = "block-bench")]
fn select_blank_range_test_lba(
    tag: &str,
    dev: &Arc<BlockDevice>,
    backend: &SyncBlockBackend,
    features: BlockFeatures,
) -> Option<u64> {
    let total = dev.geometry().block_count()?;
    if total == 0 {
        return None;
    }
    let alignment = range_validation_alignment_blocks(tag, dev, features)?;
    let last_lba = total.saturating_sub(1);
    let candidate_lba = last_lba - (last_lba % alignment);
    let first_zero = read_probe_block_is_zero(backend, 0).ok()?;
    let candidate_zero = read_probe_block_is_zero(backend, candidate_lba).ok()?;
    if !first_zero || !candidate_zero {
        log::warning!(
            "[bench][{}][range-op] skipped: unmounted device is not blank enough first_zero={} candidate_lba={} candidate_zero={}",
            tag,
            first_zero,
            candidate_lba,
            candidate_zero
        );
        return None;
    }
    Some(candidate_lba)
}

#[cfg(feature = "block-bench")]
fn range_validation_alignment_blocks(
    tag: &str,
    dev: &Arc<BlockDevice>,
    features: BlockFeatures,
) -> Option<u64> {
    let mut alignment = 1u64;
    if features.contains(BlockFeatures::WRITE_ZEROES) {
        alignment = merge_range_alignment(tag, dev, alignment, BioOp::WriteZeroes)?;
    }
    if features.contains(BlockFeatures::DISCARD) {
        alignment = merge_range_alignment(tag, dev, alignment, BioOp::Discard)?;
    }
    Some(alignment.max(1))
}

#[cfg(feature = "block-bench")]
fn merge_range_alignment(
    tag: &str,
    dev: &Arc<BlockDevice>,
    current: u64,
    op: BioOp,
) -> Option<u64> {
    let Some(limits) = dev.limits().range_limits_for(op) else {
        log::warning!(
            "[bench][{}][range-op] skipped: {:?} feature lacks range limits",
            tag,
            op
        );
        return None;
    };
    if let Some(max_blocks) = limits.max_blocks_per_io() {
        if max_blocks.get() == 0 {
            return None;
        }
    }
    let next = limits
        .alignment_blocks()
        .map(|alignment| u64::from(alignment.get()))
        .unwrap_or(1);
    lcm_u64(current.max(1), next.max(1))
}

#[cfg(feature = "block-bench")]
fn lcm_u64(a: u64, b: u64) -> Option<u64> {
    a.checked_div(gcd_u64(a, b))?.checked_mul(b)
}

#[cfg(feature = "block-bench")]
fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let next = a % b;
        a = b;
        b = next;
    }
    a.max(1)
}

#[cfg(feature = "block-bench")]
fn read_probe_block_is_zero(backend: &SyncBlockBackend, lba: u64) -> Result<bool, ()> {
    let len = backend.sector_size_bytes() as usize;
    let mut buf = vec![0u8; len];
    backend.read(lba, 1, &mut buf).map_err(|_| ())?;
    Ok(buf.iter().all(|byte| *byte == 0))
}

#[cfg(feature = "block-bench")]
fn range_validation_pattern(len: usize) -> Vec<u8> {
    let mut data = vec![0u8; len];
    for (idx, byte) in data.iter_mut().enumerate() {
        *byte = 0x5au8 ^ (idx as u8).wrapping_mul(17);
    }
    data
}

#[cfg(feature = "block-bench")]
fn submit_range_op(
    dev: &Arc<BlockDevice>,
    op: BioOp,
    lba: u64,
) -> Result<(), general::dev::bio::BioError> {
    dev.submit_bio_wait(op, BlockRange { lba, blocks: 1 }, BioBuffer::None)
        .map(|_| ())
}

#[cfg(feature = "block-bench")]
fn restore_range_probe_block(
    tag: &str,
    dev: &Arc<BlockDevice>,
    backend: &SyncBlockBackend,
    lba: u64,
    original: &[u8],
) {
    if let Err(err) = backend.write(lba, 1, original) {
        log::error!(
            "[bench][{}][range-op] failed to restore probe block lba={}: {:?}",
            tag,
            lba,
            err
        );
        return;
    }
    if let Err(err) = dev.control(BlockControlRequest::Flush) {
        log::error!(
            "[bench][{}][range-op] failed to flush restored probe block lba={}: {:?}",
            tag,
            lba,
            err
        );
    }
}

#[cfg(any(feature = "bench", feature = "block-bench"))]
fn run_block_seq_read(tag: &str, dev: &Arc<BlockDevice>) {
    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][{}][L1-blk] backend unavailable: {:?}", tag, e);
            return;
        }
    };
    let chunk = 1024 * 1024usize;
    let total_bytes = 4 * 1024 * 1024usize;
    let lbs = dev.geometry().logical_block_size().get() as usize;
    if chunk % lbs != 0 {
        return;
    }
    if !block_device_has_bytes(dev, total_bytes) {
        log::error!(
            "[bench][{}][L1-blk] device too small for {} bytes seq read",
            tag,
            total_bytes
        );
        return;
    }
    let blocks_per_chunk = (chunk / lbs) as u32;
    let iters = total_bytes / chunk;
    let mut buf = vec![0u8; chunk];
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        if backend.read(lba, blocks_per_chunk, &mut buf).is_err() {
            log::error!("[bench][{}][L1-blk] seq read error", tag);
            return;
        }
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let mibps = (total_bytes as u64) * 1_000_000_000 / dt.max(1) / (1024 * 1024);
    log::info!(
        "[bench][{}][L1-blk] seq read 4 MiB in {} ns ({} MiB/s)  <-- 裸块层开销",
        tag,
        dt,
        mibps
    );
}

#[cfg(feature = "block-bench")]
fn run_block_seq_read_repeat(tag: &str, dev: &Arc<BlockDevice>) {
    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][{}][L1-repeat] backend unavailable: {:?}", tag, e);
            return;
        }
    };
    let chunk = 1024 * 1024usize;
    let total_bytes = 4 * 1024 * 1024usize;
    let lbs = dev.geometry().logical_block_size().get() as usize;
    if chunk % lbs != 0 || !block_device_has_bytes(dev, total_bytes) {
        return;
    }
    let blocks_per_chunk = (chunk / lbs) as u32;
    let iters = total_bytes / chunk;
    let mut buf = vec![0u8; chunk];

    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        let _ = backend.read(lba, blocks_per_chunk, &mut buf);
    }
    core::hint::black_box(&buf);

    let before = dev.io_stats();
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        if backend.read(lba, blocks_per_chunk, &mut buf).is_err() {
            log::error!("[bench][{}][L1-repeat] seq read error", tag);
            return;
        }
    }
    core::hint::black_box(&buf);
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let after = dev.io_stats();
    let read_ios = after.read_ios.saturating_sub(before.read_ios);
    let read_sectors = after.read_sectors.saturating_sub(before.read_sectors);
    let read_time_ns = after.read_time_ns.saturating_sub(before.read_time_ns);
    let mibps = (total_bytes as u64) * 1_000_000_000 / dt.max(1) / (1024 * 1024);
    log::info!(
        "[bench][{}][L1-repeat] seq read 4 MiB repeat: {} ns ({} MiB/s) backend_ios={} backend_read={} KiB backend_avg={} ns",
        tag,
        dt,
        mibps,
        read_ios,
        read_sectors / 2,
        read_time_ns / read_ios.max(1)
    );
}

/// 精确对比测试：同一设备、同一数据、相同 cache 状态下，
/// 对比「直接 memcpy」vs「通过 SyncBlockBackend 完整路径」。
/// 先 warmup 把数据拉入 cache，再分别测量两条路径。
#[cfg(feature = "bench")]
fn run_block_seq_read_instrumented(tag: &str, dev: &Arc<BlockDevice>) {
    let chunk = 1024 * 1024usize;
    let total_bytes = 4 * 1024 * 1024usize;
    let lbs = dev.geometry().logical_block_size().get() as usize;
    if chunk % lbs != 0 {
        return;
    }
    let blocks_per_chunk = (chunk / lbs) as u32;
    let iters = total_bytes / chunk;

    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][{}][PROOF] backend unavailable: {:?}", tag, e);
            return;
        }
    };
    let mut buf = vec![0u8; chunk];

    // ── Warmup: 通过完整路径读一遍，把 backing store 拉入 cache ──
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        let _ = backend.read(lba, blocks_per_chunk, &mut buf);
    }
    core::hint::black_box(&buf);

    // ── 测量 A: 完整路径（cache 已热） ──
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        let _ = backend.read(lba, blocks_per_chunk, &mut buf);
    }
    core::hint::black_box(&buf);
    let dt_full = hal::time::monotonic_ns().saturating_sub(t0);

    // ── 测量 B: 重复完整路径。旧 read_sectors_sync 快路径已移除，这里不是直连驱动路径。 ──
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        let _ = backend.read(lba, blocks_per_chunk, &mut buf);
    }
    core::hint::black_box(&buf);
    let dt_repeat = hal::time::monotonic_ns().saturating_sub(t0);

    // ── 测量 C: 纯 memcpy（同一 backing store，手动 lock） ──
    let io_ref = dev.downcast_driver::<RamBlockIo>().unwrap();
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let off = i * chunk;
        let guard = io_ref.data.lock();
        buf[..chunk].copy_from_slice(&guard[off..off + chunk]);
        core::hint::black_box(&buf);
        drop(guard);
    }
    let dt_raw = hal::time::monotonic_ns().saturating_sub(t0);

    let backend_over_raw = dt_full.saturating_sub(dt_raw);
    log::info!("[bench][{}][PROOF] same device, cache hot, 4 MiB x3:", tag);
    log::info!(
        "[bench][{}][PROOF]   SyncBlockBackend pass#1: {} ns ({} MiB/s)",
        tag,
        dt_full,
        (total_bytes as u64) * 1_000_000_000 / dt_full.max(1) / (1024 * 1024)
    );
    log::info!(
        "[bench][{}][PROOF]   SyncBlockBackend pass#2: {} ns ({} MiB/s)",
        tag,
        dt_repeat,
        (total_bytes as u64) * 1_000_000_000 / dt_repeat.max(1) / (1024 * 1024)
    );
    log::info!(
        "[bench][{}][PROOF]   raw lock+memcpy: {} ns ({} MiB/s)",
        tag,
        dt_raw,
        (total_bytes as u64) * 1_000_000_000 / dt_raw.max(1) / (1024 * 1024)
    );
    log::info!(
        "[bench][{}][PROOF]   backend wrapper over raw memcpy: {} ns ({} ns/op); no direct read_sectors_sync path measured",
        tag,
        backend_over_raw,
        backend_over_raw / iters as u64
    );
}

#[cfg(feature = "bench")]
fn run_block_overhead_breakdown(tag: &str, dev: &Arc<BlockDevice>) {
    let io_ref = match dev.downcast_driver::<RamBlockIo>() {
        Some(r) => r,
        None => {
            log::error!("[bench][{}][OVERHEAD] not a RamBlockIo device", tag);
            return;
        }
    };
    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(b) => b,
        Err(e) => {
            log::error!("[bench][{}][OVERHEAD] backend unavailable: {:?}", tag, e);
            return;
        }
    };
    let lbs = dev.geometry().logical_block_size().get() as usize;

    run_overhead_suite(tag, dev, io_ref, &backend, lbs, 1024 * 1024, "1MiB");
    run_overhead_suite(tag, dev, io_ref, &backend, lbs, 4096, "4K");
}

#[cfg(feature = "bench")]
fn run_overhead_suite(
    tag: &str,
    dev: &Arc<BlockDevice>,
    io_ref: &RamBlockIo,
    backend: &SyncBlockBackend,
    lbs: usize,
    chunk: usize,
    chunk_label: &str,
) {
    let total_bytes = 4 * 1024 * 1024usize;
    let iters = total_bytes / chunk;
    let blocks_per_chunk = (chunk / lbs) as u32;
    let mut buf = vec![0u8; chunk];

    // warmup
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        let _ = backend.read(lba, blocks_per_chunk, &mut buf);
    }
    core::hint::black_box(&buf);

    log::info!(
        "[bench][{}][OVERHEAD] ── {} x {} ({} total) ──",
        tag,
        chunk_label,
        iters,
        total_bytes
    );

    // ── L0: pure memcpy (no lock, no abstraction) ──
    let src = vec![0xABu8; chunk];
    let t0 = hal::time::monotonic_ns();
    for _ in 0..iters {
        buf[..chunk].copy_from_slice(&src);
        core::hint::black_box(&buf);
    }
    let dt_l0 = hal::time::monotonic_ns().saturating_sub(t0);

    // ── L1: spinlock + memcpy (driver data access only) ──
    let backing_len = io_ref.data.lock().len();
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let off = (i * chunk) % backing_len.max(chunk);
        let guard = io_ref.data.lock();
        let end = (off + chunk).min(guard.len());
        buf[..end - off].copy_from_slice(&guard[off..end]);
        core::hint::black_box(&buf);
        drop(guard);
    }
    let dt_l1 = hal::time::monotonic_ns().saturating_sub(t0);

    // ── L2: queue_bio direct (Bio + Completion alloc + driver dispatch, reuse buffer) ──
    let block_size = dev.geometry().logical_block_size();
    let mut owned_buf: Box<[u8]> = vec![0u8; chunk].into_boxed_slice();
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        let buffer = BioBuffer::Owned(owned_buf);
        let range = BlockRange {
            lba,
            blocks: blocks_per_chunk,
        };
        let completion = Completion::<BioResult>::new();
        let bio = Bio::new_with_completion_observer(
            BioOp::Read,
            range,
            buffer,
            block_size,
            0,
            None,
            Arc::clone(&completion),
        );
        let _ = io_ref.queue_bio(bio);
        match completion.wait() {
            Ok(bio) => {
                if let BioBuffer::Owned(b) = bio.buffer {
                    owned_buf = b;
                } else {
                    owned_buf = vec![0u8; chunk].into_boxed_slice();
                }
            }
            Err(_) => {
                owned_buf = vec![0u8; chunk].into_boxed_slice();
            }
        }
    }
    let dt_l2 = hal::time::monotonic_ns().saturating_sub(t0);

    // ── L4: full SyncBlockBackend path (validate + observer + completion) ──
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        let _ = backend.read(lba, blocks_per_chunk, &mut buf);
    }
    core::hint::black_box(&buf);
    let dt_l4 = hal::time::monotonic_ns().saturating_sub(t0);

    // ── L5: pure Completion alloc+drop cost ──
    let t0 = hal::time::monotonic_ns();
    for _ in 0..iters {
        let c = Completion::<BioResult>::new();
        core::hint::black_box(&c);
        drop(c);
    }
    let dt_l5 = hal::time::monotonic_ns().saturating_sub(t0);

    // ── L6: pure Box<[u8]> alloc+drop cost ──
    let t0 = hal::time::monotonic_ns();
    for _ in 0..iters {
        let b: Box<[u8]> = vec![0u8; chunk].into_boxed_slice();
        core::hint::black_box(&b);
        drop(b);
    }
    let dt_l6 = hal::time::monotonic_ns().saturating_sub(t0);

    // ── Report ──
    let mib = |dt: u64| (total_bytes as u64) * 1_000_000_000 / dt.max(1) / (1024 * 1024);
    let per_op = |dt: u64| dt / iters as u64;

    log::info!(
        "[bench][{}][OVERHEAD]   L0 pure memcpy:       {} ns ({} MiB/s, {} ns/op)",
        tag,
        dt_l0,
        mib(dt_l0),
        per_op(dt_l0)
    );
    log::info!(
        "[bench][{}][OVERHEAD]   L1 lock+memcpy:       {} ns ({} MiB/s, {} ns/op)",
        tag,
        dt_l1,
        mib(dt_l1),
        per_op(dt_l1)
    );
    log::info!(
        "[bench][{}][OVERHEAD]   L2 queue_bio direct:  {} ns ({} MiB/s, {} ns/op)",
        tag,
        dt_l2,
        mib(dt_l2),
        per_op(dt_l2)
    );
    log::info!(
        "[bench][{}][OVERHEAD]   L4 full backend path: {} ns ({} MiB/s, {} ns/op)",
        tag,
        dt_l4,
        mib(dt_l4),
        per_op(dt_l4)
    );
    log::info!(
        "[bench][{}][OVERHEAD]   L5 Completion alloc:  {} ns ({} ns/op)",
        tag,
        dt_l5,
        per_op(dt_l5)
    );
    log::info!(
        "[bench][{}][OVERHEAD]   L6 Box<[u8]> alloc:   {} ns ({} ns/op)",
        tag,
        dt_l6,
        per_op(dt_l6)
    );

    log::info!(
        "[bench][{}][OVERHEAD] ── breakdown ({}) ──",
        tag,
        chunk_label
    );
    log::info!(
        "[bench][{}][OVERHEAD]   spinlock cost:       {} ns/op (L1-L0)",
        tag,
        per_op(dt_l1.saturating_sub(dt_l0))
    );
    log::info!(
        "[bench][{}][OVERHEAD]   bio+completion+drv:  {} ns/op (L2-L1)",
        tag,
        per_op(dt_l2.saturating_sub(dt_l1))
    );
    log::info!(
        "[bench][{}][OVERHEAD]   validate+observer:   {} ns/op (L4-L2)",
        tag,
        per_op(dt_l4.saturating_sub(dt_l2))
    );
    log::info!(
        "[bench][{}][OVERHEAD]   alloc overhead only: {} ns/op (L5+L6)",
        tag,
        per_op(dt_l5 + dt_l6)
    );
}

#[cfg(feature = "bench")]
fn run_block_seq_write(tag: &str, dev: &Arc<BlockDevice>) {
    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][{}][L1-blk] backend unavailable: {:?}", tag, e);
            return;
        }
    };
    let chunk = 1024 * 1024usize;
    let total_bytes = 4 * 1024 * 1024usize;
    let lbs = dev.geometry().logical_block_size().get() as usize;
    if chunk % lbs != 0 {
        return;
    }
    let blocks_per_chunk = (chunk / lbs) as u32;
    let iters = total_bytes / chunk;
    let buf = vec![0x55; chunk];
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        if backend.write(lba, blocks_per_chunk, &buf).is_err() {
            log::error!("[bench][{}][L1-blk] seq write error", tag);
            return;
        }
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let mibps = (total_bytes as u64) * 1_000_000_000 / dt.max(1) / (1024 * 1024);
    log::info!(
        "[bench][{}][L1-blk] seq write 4 MiB in {} ns ({} MiB/s)  <-- 裸块层开销",
        tag,
        dt,
        mibps
    );
}

// ═══════════════════════════════════════════════════════════════════════
// 块设备软件开销诊断（适用于真实硬件设备如 virtio-blk）
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "block-bench")]
fn run_block_overhead_diagnosis(tag: &str, dev: &Arc<BlockDevice>) {
    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(b) => b,
        Err(e) => {
            log::error!("[bench][{}][DIAG] backend unavailable: {:?}", tag, e);
            return;
        }
    };
    let lbs = dev.geometry().logical_block_size().get() as usize;
    let total_bytes = 4 * 1024 * 1024usize;
    if !block_device_has_bytes(dev, total_bytes) {
        log::warning!("[bench][{}][DIAG] device too small for diagnosis", tag);
        return;
    }

    log::info!(
        "[bench][{}][DIAG] ====== block device overhead diagnosis ======",
        tag
    );
    log::info!("[bench][{}][DIAG] hardware logical_block_size={}", tag, lbs);

    // ── Part A: multi-size sequential read — 测固定 vs 比例开销 ──
    log::info!(
        "[bench][{}][DIAG] -- Part A: per-op latency vs I/O size --",
        tag
    );
    let sizes: &[usize] = &[512, 4096, 65536, 1024 * 1024];
    for &chunk in sizes {
        if chunk < lbs || chunk % lbs != 0 {
            continue;
        }
        run_diag_chunk(tag, dev, &backend, lbs, chunk, total_bytes);
    }

    // ── Part B: 软件常量开销（独立于硬件） ──
    log::info!(
        "[bench][{}][DIAG] -- Part B: pure software constants --",
        tag
    );
    let n = 5000u64;

    // B1: Completion alloc + drop
    let t0 = hal::time::monotonic_ns();
    for _ in 0..n {
        let c = Completion::<()>::new();
        core::hint::black_box(&c);
        drop(c);
    }
    let dt_completion = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][{}][DIAG]   Completion<()>::new+drop: {} ns/op (n={})",
        tag,
        dt_completion / n,
        n
    );

    // B2: 2x now_ns_public — 同步路径每次 I/O 都至少调 2 次
    let t0 = hal::time::monotonic_ns();
    let mut sink = 0u64;
    for _ in 0..n {
        let a = sched::now_ns_public();
        let b = sched::now_ns_public();
        sink = sink.wrapping_add(a ^ b);
    }
    core::hint::black_box(sink);
    let dt_now2 = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][{}][DIAG]   2x now_ns_public:         {} ns/op (n={})",
        tag,
        dt_now2 / n,
        n
    );

    // B3: vec! buffer alloc — 文件系统每次 read_sectors 可能触发
    let alloc_size = 4096usize;
    let t0 = hal::time::monotonic_ns();
    for _ in 0..n {
        let v: Vec<u8> = vec![0u8; alloc_size];
        core::hint::black_box(&v);
        drop(v);
    }
    let dt_alloc = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][{}][DIAG]   vec![0u8; 4096] +drop:    {} ns/op (n={})",
        tag,
        dt_alloc / n,
        n
    );

    log::info!("[bench][{}][DIAG] ====== diagnosis complete ======", tag);
}

#[cfg(feature = "block-bench")]
fn run_diag_chunk(
    tag: &str,
    dev: &Arc<BlockDevice>,
    backend: &SyncBlockBackend,
    lbs: usize,
    chunk: usize,
    total_bytes: usize,
) {
    let blocks_per_chunk = (chunk / lbs) as u32;
    let iters = (total_bytes / chunk) as u64;
    if iters == 0 {
        return;
    }
    let mut buf = vec![0u8; chunk];

    // warmup
    for i in 0..iters {
        let lba = i * blocks_per_chunk as u64;
        let _ = backend.read(lba, blocks_per_chunk, &mut buf);
    }
    core::hint::black_box(&buf);

    // timed run
    let before = dev.io_stats();
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = i * blocks_per_chunk as u64;
        if backend.read(lba, blocks_per_chunk, &mut buf).is_err() {
            log::error!("[bench][{}][DIAG] read error at chunk={}", tag, chunk);
            return;
        }
    }
    core::hint::black_box(&buf);
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let after = dev.io_stats();
    let read_ios = after.read_ios.saturating_sub(before.read_ios).max(1);
    let hw_time = after.read_time_ns.saturating_sub(before.read_time_ns);

    let wall_per_op = dt / iters;
    let hw_per_op = hw_time / read_ios;
    let overhead_per_op = wall_per_op.saturating_sub(hw_per_op);
    let overhead_pct = if wall_per_op > 0 {
        overhead_per_op * 100 / wall_per_op
    } else {
        0
    };
    let mibps = (chunk as u64 * iters) * 1_000_000_000 / dt.max(1) / (1024 * 1024);

    log::info!(
        "[bench][{}][DIAG]   chunk={:>7}B iters={:>4} wall={:>8}ns/op  hw={:>8}ns/op  overhead={:>8}ns/op ({:>2}%)  thr={} MiB/s",
        tag,
        chunk,
        iters,
        wall_per_op,
        hw_per_op,
        overhead_per_op,
        overhead_pct,
        mibps
    );
}

// ═══════════════════════════════════════════════════════════════════════
// L2: 裸块设备随机读取
// ═══════════════════════════════════════════════════════════════════════

#[cfg(any(feature = "bench", feature = "block-bench"))]
fn run_block_rand_read(tag: &str, dev: &Arc<BlockDevice>) {
    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][{}][L2-blk] backend unavailable: {:?}", tag, e);
            return;
        }
    };
    let lbs = dev.geometry().logical_block_size().get() as usize;
    let block = 4096usize;
    let count = 100u64;
    if block % lbs != 0 {
        return;
    }
    let blocks_per_op = (block / lbs) as u32;
    let stride = blocks_per_op as u64;
    let Some(max_lba) = dev
        .geometry()
        .block_count()
        .and_then(|total| total.checked_sub(blocks_per_op as u64))
    else {
        return;
    };
    if max_lba == 0 {
        return;
    }
    let mut buf = vec![0u8; block];
    let mut rng = 0xdeadbeefu32;
    let t0 = hal::time::monotonic_ns();
    for _ in 0..count {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        let lba = (rng as u64) % (max_lba / stride + 1) * stride;
        let _ = backend.read(lba, blocks_per_op, &mut buf);
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let avg_ns = dt / count;
    log::info!(
        "[bench][{}][L2-blk] rand read {} x 4 KiB: total {} ns (avg {} ns/op)  <-- 裸块随机延迟",
        tag,
        count,
        dt,
        avg_ns
    );
}

// ═══════════════════════════════════════════════════════════════════════
// FAT 写路径细化插桩
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "bench")]
fn run_fat_write_breakdown(tag: &str, sb: &Arc<Superblock>) {
    let root = &sb.root_inode;
    let cred = Credentials::root();
    let total = 4 * 1024 * 1024usize;
    let wbuf = vec![0xDD; total];

    log::info!(
        "[bench][{}][FAT-BREAKDOWN] ---- write path analysis ----",
        tag
    );

    // ── Test 1: 单次 4 MiB write（grow_to + write 一起） ──
    {
        let fname = "._bk1_";
        if let Ok(c) = root.lookup(fname) {
            let _ = root.unlink(fname, &*c);
        }
        let t0 = hal::time::monotonic_ns();
        let inode = root
            .create(fname, vfs::stat::FileMode::new(0o644), &cred)
            .unwrap();
        let dt_create = hal::time::monotonic_ns().saturating_sub(t0);

        let opts = OpenOptions {
            access: vfs::file::AccessMode::WriteOnly,
            ..OpenOptions::default()
        };
        let f = inode.open_ops(&opts, &cred).unwrap();
        let t0 = hal::time::monotonic_ns();
        let _ = f.write_at(&wbuf, 0);
        let dt_write = hal::time::monotonic_ns().saturating_sub(t0);
        drop(f);

        log::info!(
            "[bench][{}][FAT-BREAKDOWN] 1x4MiB: create {} ns | write {} ns (total {} ns, {} MiB/s)",
            tag,
            dt_create,
            dt_write,
            dt_create + dt_write,
            (total as u64) * 1_000_000_000 / dt_write.max(1) / (1024 * 1024)
        );
        if let Ok(c) = root.lookup(fname) {
            let _ = root.unlink(fname, &*c);
        }
    }

    // ── Test 2: 预分配后纯写（分离 grow_to 和 data write） ──
    {
        let fname = "._bk2_";
        if let Ok(c) = root.lookup(fname) {
            let _ = root.unlink(fname, &*c);
        }
        let inode = root
            .create(fname, vfs::stat::FileMode::new(0o644), &cred)
            .unwrap();
        let opts = OpenOptions {
            access: vfs::file::AccessMode::WriteOnly,
            ..OpenOptions::default()
        };
        let f = inode.open_ops(&opts, &cred).unwrap();

        // 先写 1 字节触发 grow_to 4 MiB（预分配全部簇）
        let grow_buf = vec![0u8; total];
        let t0 = hal::time::monotonic_ns();
        let _ = f.write_at(&grow_buf, 0);
        let dt_first_write = hal::time::monotonic_ns().saturating_sub(t0);

        // 再覆盖写同一区域（簇已分配，纯数据写入）
        let t0 = hal::time::monotonic_ns();
        let _ = f.write_at(&wbuf, 0);
        let dt_overwrite = hal::time::monotonic_ns().saturating_sub(t0);
        drop(f);

        let dt_grow = dt_first_write.saturating_sub(dt_overwrite);
        let pct_grow = dt_grow * 100 / dt_first_write.max(1);
        let pct_data = dt_overwrite * 100 / dt_first_write.max(1);
        log::info!(
            "[bench][{}][FAT-BREAKDOWN] first write (grow+data): {} ns | overwrite (data only): {} ns",
            tag,
            dt_first_write,
            dt_overwrite
        );
        log::info!(
            "[bench][{}][FAT-BREAKDOWN] => grow_to overhead: ~{} ns ({}%) | pure data: ~{} ns ({}%)",
            tag,
            dt_grow,
            pct_grow,
            dt_overwrite,
            pct_data
        );
        if let Ok(c) = root.lookup(fname) {
            let _ = root.unlink(fname, &*c);
        }
    }

    // ── Test 3: 不同块大小写入（看 per-call 开销） ──
    {
        let fname = "._bk3_";
        if let Ok(c) = root.lookup(fname) {
            let _ = root.unlink(fname, &*c);
        }
        let inode = root
            .create(fname, vfs::stat::FileMode::new(0o644), &cred)
            .unwrap();
        let opts = OpenOptions {
            access: vfs::file::AccessMode::WriteOnly,
            ..OpenOptions::default()
        };
        let f = inode.open_ops(&opts, &cred).unwrap();

        // 先预分配
        let _ = f.write_at(&vec![0u8; total], 0);

        // 用 4 KiB 块覆盖写
        let chunk = 4096usize;
        let iters = total / chunk;
        let small_buf = vec![0xEE; chunk];
        let t0 = hal::time::monotonic_ns();
        for i in 0..iters {
            let _ = f.write_at(&small_buf, (i * chunk) as u64);
        }
        let dt_4k = hal::time::monotonic_ns().saturating_sub(t0);

        // 用 1 MiB 块覆盖写
        let chunk_1m = 1024 * 1024usize;
        let iters_1m = total / chunk_1m;
        let big_buf = vec![0xFF; chunk_1m];
        let t0 = hal::time::monotonic_ns();
        for i in 0..iters_1m {
            let _ = f.write_at(&big_buf, (i * chunk_1m) as u64);
        }
        let dt_1m = hal::time::monotonic_ns().saturating_sub(t0);
        drop(f);

        let avg_4k = dt_4k / iters as u64;
        let avg_1m = dt_1m / iters_1m as u64;
        log::info!(
            "[bench][{}][FAT-BREAKDOWN] overwrite 4MiB as 4K x {}: {} ns (avg {} ns/call, {} MiB/s)",
            tag,
            iters,
            dt_4k,
            avg_4k,
            (total as u64) * 1_000_000_000 / dt_4k.max(1) / (1024 * 1024)
        );
        log::info!(
            "[bench][{}][FAT-BREAKDOWN] overwrite 4MiB as 1M x {}: {} ns (avg {} ns/call, {} MiB/s)",
            tag,
            iters_1m,
            dt_1m,
            avg_1m,
            (total as u64) * 1_000_000_000 / dt_1m.max(1) / (1024 * 1024)
        );
        if let Ok(c) = root.lookup(fname) {
            let _ = root.unlink(fname, &*c);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// EXT4 写路径细化插桩
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "bench")]
fn run_ext_write_breakdown(tag: &str, sb: &Arc<Superblock>) {
    let root = &sb.root_inode;
    let cred = Credentials::root();
    let total = 4 * 1024 * 1024usize;
    let wbuf = vec![0xDD; total];

    log::info!(
        "[bench][{}][EXT-BREAKDOWN] ---- write path analysis ----",
        tag
    );

    // ── Test 1: 单次 4 MiB write（alloc + data） ──
    {
        let fname = "._ek1_";
        if let Ok(c) = root.lookup(fname) {
            let _ = root.unlink(fname, &*c);
        }
        let t0 = hal::time::monotonic_ns();
        let inode = root
            .create(fname, vfs::stat::FileMode::new(0o644), &cred)
            .unwrap();
        let dt_create = hal::time::monotonic_ns().saturating_sub(t0);

        let opts = OpenOptions {
            access: vfs::file::AccessMode::WriteOnly,
            ..OpenOptions::default()
        };
        let f = inode.open_ops(&opts, &cred).unwrap();
        let t0 = hal::time::monotonic_ns();
        let _ = f.write_at(&wbuf, 0);
        let dt_write = hal::time::monotonic_ns().saturating_sub(t0);
        drop(f);

        log::info!(
            "[bench][{}][EXT-BREAKDOWN] 1x4MiB: create {} ns | write {} ns ({} MiB/s)",
            tag,
            dt_create,
            dt_write,
            (total as u64) * 1_000_000_000 / dt_write.max(1) / (1024 * 1024)
        );
        if let Ok(c) = root.lookup(fname) {
            let _ = root.unlink(fname, &*c);
        }
    }

    // ── Test 2: 预分配后纯覆盖写 ──
    {
        let fname = "._ek2_";
        if let Ok(c) = root.lookup(fname) {
            let _ = root.unlink(fname, &*c);
        }
        let inode = root
            .create(fname, vfs::stat::FileMode::new(0o644), &cred)
            .unwrap();
        let opts = OpenOptions {
            access: vfs::file::AccessMode::WriteOnly,
            ..OpenOptions::default()
        };
        let f = inode.open_ops(&opts, &cred).unwrap();

        let grow_buf = vec![0u8; total];
        let t0 = hal::time::monotonic_ns();
        let _ = f.write_at(&grow_buf, 0);
        let dt_first = hal::time::monotonic_ns().saturating_sub(t0);

        let t0 = hal::time::monotonic_ns();
        let _ = f.write_at(&wbuf, 0);
        let dt_over = hal::time::monotonic_ns().saturating_sub(t0);

        let dt_alloc = dt_first.saturating_sub(dt_over);
        let pct_alloc = dt_alloc * 100 / dt_first.max(1);
        let pct_data = dt_over * 100 / dt_first.max(1);
        log::info!(
            "[bench][{}][EXT-BREAKDOWN] first write: {} ns | overwrite: {} ns",
            tag,
            dt_first,
            dt_over
        );
        log::info!(
            "[bench][{}][EXT-BREAKDOWN] => alloc: ~{} ns ({}%) | data: ~{} ns ({}%)",
            tag,
            dt_alloc,
            pct_alloc,
            dt_over,
            pct_data
        );

        // 覆盖写 per-call 开销
        let small = vec![0xEE; 4096];
        let t0 = hal::time::monotonic_ns();
        for i in 0..1024 {
            let _ = f.write_at(&small, (i * 4096) as u64);
        }
        let dt_4k = hal::time::monotonic_ns().saturating_sub(t0);

        // 细分：再做一轮 4K overwrite，分段计时
        let mut dt_4k_warm = 0u64;
        let t0 = hal::time::monotonic_ns();
        for i in 0..1024 {
            let _ = f.write_at(&small, (i * 4096) as u64);
        }
        dt_4k_warm = hal::time::monotonic_ns().saturating_sub(t0);
        log::info!(
            "[bench][{}][EXT-BREAKDOWN] overwrite 4K x 1024 (cold): {} ns (avg {} ns/call)",
            tag,
            dt_4k,
            dt_4k / 1024
        );
        log::info!(
            "[bench][{}][EXT-BREAKDOWN] overwrite 4K x 1024 (warm): {} ns (avg {} ns/call)",
            tag,
            dt_4k_warm,
            dt_4k_warm / 1024
        );

        let big = vec![0xFF; 1024 * 1024];
        let t0 = hal::time::monotonic_ns();
        for i in 0..4u64 {
            let _ = f.write_at(&big, i * 1024 * 1024);
        }
        let dt_1m = hal::time::monotonic_ns().saturating_sub(t0);
        drop(f);

        log::info!(
            "[bench][{}][EXT-BREAKDOWN] overwrite 4K x 1024: {} ns (avg {} ns/call, {} MiB/s)",
            tag,
            dt_4k,
            dt_4k / 1024,
            (total as u64) * 1_000_000_000 / dt_4k.max(1) / (1024 * 1024)
        );
        log::info!(
            "[bench][{}][EXT-BREAKDOWN] overwrite 1M x 4: {} ns (avg {} ns/call, {} MiB/s)",
            tag,
            dt_1m,
            dt_1m / 4,
            (total as u64) * 1_000_000_000 / dt_1m.max(1) / (1024 * 1024)
        );
        if let Ok(c) = root.lookup(fname) {
            let _ = root.unlink(fname, &*c);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// L6: 随机读文件
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "block-bench")]
fn run_block_device_fs_read_suite(tag: &str, dev: &Arc<BlockDevice>, sb: &Arc<Superblock>) {
    log::info!(
        "[bench][{}] read-only fs bench fs={} block_size={}",
        tag,
        sb.fs_type,
        sb.block_size
    );
    run_fs_meta_readonly(tag, sb);
    run_fs_seq_read_existing(tag, sb, dev);
    run_fs_rand_read_existing(tag, sb, dev);
}

#[cfg(feature = "block-bench")]
fn run_fs_seq_read_existing(tag: &str, sb: &Arc<Superblock>, dev: &Arc<BlockDevice>) {
    let root = &sb.root_inode;
    let cred = Credentials::root();
    let Some((name, _inode, file, size)) = find_largest_regular_file(root, &cred) else {
        log::warning!(
            "[bench][{}][L5-read] no regular file in root directory",
            tag
        );
        return;
    };
    let want = core::cmp::min(size, 4 * 1024 * 1024) as usize;
    if want == 0 {
        log::warning!("[bench][{}][L5-read] selected file {} is empty", tag, name);
        return;
    }

    let mut buf = vec![0u8; want];
    let before = dev.io_stats();
    let t0 = hal::time::monotonic_ns();
    let n = match file.read_at(&mut buf, 0) {
        Ok(n) => n,
        Err(e) => {
            log::error!("[bench][{}][L5-read] {} read failed: {:?}", tag, name, e);
            return;
        }
    };
    core::hint::black_box(&buf[..n.min(want)]);
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let after = dev.io_stats();
    let read_ios = after.read_ios.saturating_sub(before.read_ios);
    let read_sectors = after.read_sectors.saturating_sub(before.read_sectors);
    let read_time_ns = after.read_time_ns.saturating_sub(before.read_time_ns);
    let mibps = (n as u64) * 1_000_000_000 / dt.max(1) / (1024 * 1024);
    log::info!(
        "[bench][{}][L5-read] existing file {} size={} read={} bytes: {} ns ({} MiB/s) backend_ios={} backend_read={} KiB backend_avg={} ns",
        tag,
        name,
        size,
        n,
        dt,
        mibps,
        read_ios,
        read_sectors / 2,
        read_time_ns / read_ios.max(1)
    );
}

#[cfg(feature = "block-bench")]
fn run_fs_rand_read_existing(tag: &str, sb: &Arc<Superblock>, dev: &Arc<BlockDevice>) {
    let root = &sb.root_inode;
    let cred = Credentials::root();
    let Some((name, _inode, file, size)) = find_largest_regular_file(root, &cred) else {
        log::warning!(
            "[bench][{}][L6-rand] no regular file in root directory",
            tag
        );
        return;
    };
    let block = 4096usize;
    if size < block as u64 {
        log::warning!(
            "[bench][{}][L6-rand] selected file {} too small: {} bytes",
            tag,
            name,
            size
        );
        return;
    }

    let count = 100usize;
    let size_usize = core::cmp::min(size, usize::MAX as u64) as usize;
    let max_off = size_usize.saturating_sub(block);
    let mut offsets = vec![0usize; count];
    let mut rng = 0xbeefdeadu32 ^ (tag.as_bytes().first().copied().unwrap_or(0) as u32);
    for off in &mut offsets {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        *off = (rng as usize) % (max_off / block + 1) * block;
    }

    log::info!(
        "[bench][{}][L6-rand] existing file {} size={} bytes",
        tag,
        name,
        size
    );
    run_fs_rand_read_pass(tag, "existing", &*file, dev, &offsets, block);
    run_fs_rand_read_pass(tag, "existing-repeat", &*file, dev, &offsets, block);
}

#[cfg(feature = "bench")]
fn run_fs_rand_read(tag: &str, sb: &Arc<Superblock>, dev: &Arc<BlockDevice>) {
    let root = &sb.root_inode;
    let cred = Credentials::root();
    let fname = ".__bench_randio__";
    let total = 4 * 1024 * 1024usize;
    let block = 4096usize;
    let count = 100usize;

    if let Ok(child) = root.lookup(fname) {
        let _ = root.unlink(fname, &*child);
    }

    let inode = match root.create(fname, vfs::stat::FileMode::new(0o644), &cred) {
        Ok(inode) => inode,
        Err(e) => {
            log::error!("[bench][{}][L6-rand] create failed: {:?}", tag, e);
            return;
        }
    };

    let opts_w = OpenOptions {
        access: vfs::file::AccessMode::WriteOnly,
        ..OpenOptions::default()
    };
    let file_w = match inode.open_ops(&opts_w, &cred) {
        Ok(file) => file,
        Err(e) => {
            log::error!("[bench][{}][L6-rand] open(W) failed: {:?}", tag, e);
            let _ = root.unlink(fname, &*inode);
            return;
        }
    };

    let mut wbuf = vec![0u8; total];
    for (idx, byte) in wbuf.iter_mut().enumerate() {
        *byte = ((idx as u32).wrapping_mul(131).wrapping_add(17) >> 3) as u8;
    }

    match file_w.write_at(&wbuf, 0) {
        Ok(n) if n == total => {}
        Ok(n) => {
            log::error!(
                "[bench][{}][L6-rand] setup short write: {} of {} bytes",
                tag,
                n,
                total
            );
            drop(file_w);
            let _ = root.unlink(fname, &*inode);
            return;
        }
        Err(e) => {
            log::error!("[bench][{}][L6-rand] setup write failed: {:?}", tag, e);
            drop(file_w);
            let _ = root.unlink(fname, &*inode);
            return;
        }
    }
    if let Err(e) = file_w.sync() {
        log::error!("[bench][{}][L6-rand] sync failed: {:?}", tag, e);
        drop(file_w);
        let _ = root.unlink(fname, &*inode);
        return;
    }
    drop(file_w);

    let file = match inode.open_ops(&OpenOptions::default(), &cred) {
        Ok(file) => file,
        Err(e) => {
            log::error!("[bench][{}][L6-rand] open(R) failed: {:?}", tag, e);
            let _ = root.unlink(fname, &*inode);
            return;
        }
    };

    let mut offsets = vec![0usize; count];
    let mut rng = 0xbeefdeadu32 ^ (tag.as_bytes().first().copied().unwrap_or(0) as u32);
    let max_off = total.saturating_sub(block);
    for off in &mut offsets {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        *off = (rng as usize) % (max_off / block + 1) * block;
    }

    run_fs_rand_read_pass(tag, "cold-ish", &*file, dev, &offsets, block);
    run_fs_rand_read_pass(tag, "repeat", &*file, dev, &offsets, block);
    drop(file);

    if let Ok(child) = root.lookup(fname) {
        let _ = root.unlink(fname, &*child);
    }
}

#[cfg(any(feature = "bench", feature = "block-bench"))]
fn run_fs_rand_read_pass(
    tag: &str,
    pass: &str,
    file: &(dyn vfs::file::FileOps + Send + Sync),
    dev: &Arc<BlockDevice>,
    offsets: &[usize],
    block: usize,
) {
    let mut buf = vec![0u8; block];
    let before = dev.io_stats();
    let mut full = 0usize;
    let mut short = 0usize;
    let mut errors = 0usize;
    let mut checksum = 0u64;

    let t0 = hal::time::monotonic_ns();
    for &off in offsets {
        match file.read_at(&mut buf, off as u64) {
            Ok(n) if n == block => {
                full += 1;
                checksum = checksum
                    .wrapping_add(buf[0] as u64)
                    .wrapping_add((buf[block / 2] as u64) << 8)
                    .wrapping_add((buf[block - 1] as u64) << 16)
                    .wrapping_add(off as u64);
                core::hint::black_box(&buf);
            }
            Ok(n) => {
                short += 1;
                checksum = checksum.wrapping_add(n as u64).wrapping_add(off as u64);
                core::hint::black_box(&buf[..n.min(block)]);
            }
            Err(_) => {
                errors += 1;
            }
        }
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let after = dev.io_stats();
    let read_ios = after.read_ios.saturating_sub(before.read_ios);
    let read_sectors = after.read_sectors.saturating_sub(before.read_sectors);
    let read_time_ns = after.read_time_ns.saturating_sub(before.read_time_ns);
    let avg_ns = dt / offsets.len().max(1) as u64;
    let backend_avg_ns = read_time_ns / read_ios.max(1);
    let backend_kib = read_sectors / 2;

    log::info!(
        "[bench][{}][L6-rand][{}] rand read {} x 4 KiB: total {} ns avg {} ns/op full={} short={} err={} backend_read_ios={} backend_read={} KiB backend_avg={} ns checksum={:#x}",
        tag,
        pass,
        offsets.len(),
        dt,
        avg_ns,
        full,
        short,
        errors,
        read_ios,
        backend_kib,
        backend_avg_ns,
        checksum
    );
}

#[cfg(feature = "block-bench")]
fn run_fs_meta_readonly(tag: &str, sb: &Arc<Superblock>) {
    let root = &sb.root_inode;
    let cred = Credentials::root();
    let t0 = hal::time::monotonic_ns();
    let dir = match root.open_ops(&OpenOptions::default(), &cred) {
        Ok(d) => d,
        Err(e) => {
            log::error!("[bench][{}][L8-meta-ro] open root failed: {:?}", tag, e);
            return;
        }
    };
    let mut count = 0u32;
    let _ = dir.readdir(0, &mut |_| {
        count += 1;
        ControlFlow::Continue(())
    });
    drop(dir);
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][{}][L8-meta-ro] readdir {} entries in {} ns",
        tag,
        count,
        dt
    );
}

#[cfg(feature = "block-bench")]
fn find_largest_regular_file(
    root: &Arc<vfs::inode::Inode>,
    cred: &Credentials,
) -> Option<(
    String,
    Arc<vfs::inode::Inode>,
    Box<dyn vfs::file::FileOps + Send + Sync>,
    u64,
)> {
    let dir = root.open_ops(&OpenOptions::default(), cred).ok()?;
    let mut entries: Vec<DirEntry> = Vec::new();
    let _ = dir.readdir(0, &mut |entry| {
        entries.push(entry);
        ControlFlow::Continue(())
    });

    let mut best_allocated: Option<(String, u64)> = None;
    let mut best_any: Option<(String, u64)> = None;
    for entry in &entries {
        if !matches!(entry.kind, vfs::stat::FileType::Regular) {
            continue;
        }
        let Ok(name) = core::str::from_utf8(entry.name.as_bytes()) else {
            continue;
        };
        if name == "." || name == ".." || name.is_empty() {
            continue;
        }
        let Ok(child) = root.lookup(name) else {
            continue;
        };
        let size = child.size();
        if size == 0 {
            continue;
        }
        if best_any
            .as_ref()
            .is_none_or(|(_, best_size)| size > *best_size)
        {
            best_any = Some((String::from(name), size));
        }
        let allocated_blocks = child.stat().map(|stat| stat.blocks).unwrap_or(0);
        if allocated_blocks != 0
            && best_allocated
                .as_ref()
                .is_none_or(|(_, best_size)| size > *best_size)
        {
            best_allocated = Some((String::from(name), size));
        }
    }

    let (name, size) = best_allocated.or(best_any)?;
    let inode = root.lookup(&name).ok()?;
    let file = inode.open_ops(&OpenOptions::default(), cred).ok()?;
    Some((name, inode, file, size))
}

// ═══════════════════════════════════════════════════════════════════════
// L5/L7: 顺序写+读（同一文件，控制变量）
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "bench")]
fn run_fs_seq_write_read(tag: &str, sb: &Arc<Superblock>) {
    let root = &sb.root_inode;
    let cred = Credentials::root();
    let fname = ".__bench_seqio__";
    let total_bytes = 4 * 1024 * 1024usize;

    // 清理残留
    if let Ok(child) = root.lookup(fname) {
        let _ = root.unlink(fname, &*child);
    }

    // 创建文件
    let inode = match root.create(fname, vfs::stat::FileMode::new(0o644), &cred) {
        Ok(i) => i,
        Err(e) => {
            log::error!("[bench][{}] create failed: {:?}", tag, e);
            return;
        }
    };
    let opts_w = OpenOptions {
        access: vfs::file::AccessMode::WriteOnly,
        ..OpenOptions::default()
    };
    let file_w = match inode.open_ops(&opts_w, &cred) {
        Ok(f) => f,
        Err(e) => {
            log::error!("[bench][{}] open(W) failed: {:?}", tag, e);
            return;
        }
    };

    // ── 顺序写 4 MiB ──
    let wbuf = vec![0xCC; total_bytes];
    let t0 = hal::time::monotonic_ns();
    if let Err(e) = file_w.write_at(&wbuf, 0) {
        log::error!("[bench][{}][L7-write] error: {:?}", tag, e);
        return;
    }
    let dt_w = hal::time::monotonic_ns().saturating_sub(t0);
    drop(file_w);
    let mibps_w = (total_bytes as u64) * 1_000_000_000 / dt_w.max(1) / (1024 * 1024);
    log::info!(
        "[bench][{}][L7-write] seq write 4 MiB: {} ns ({} MiB/s)",
        tag,
        dt_w,
        mibps_w
    );

    // ── 顺序读 4 MiB ──
    let opts_r = OpenOptions::default();
    let file_r = match inode.open_ops(&opts_r, &cred) {
        Ok(f) => f,
        Err(e) => {
            log::error!("[bench][{}] open(R) failed: {:?}", tag, e);
            return;
        }
    };
    let mut rbuf = vec![0u8; total_bytes];
    let t0 = hal::time::monotonic_ns();
    match file_r.read_at(&mut rbuf, 0) {
        Ok(n) => {
            let dt_r = hal::time::monotonic_ns().saturating_sub(t0);
            let mibps_r = (n as u64) * 1_000_000_000 / dt_r.max(1) / (1024 * 1024);
            log::info!(
                "[bench][{}][L5-read] seq read {} bytes: {} ns ({} MiB/s)",
                tag,
                n,
                dt_r,
                mibps_r
            );
        }
        Err(e) => {
            log::error!("[bench][{}][L5-read] error: {:?}", tag, e);
        }
    }
    drop(file_r);

    // 清理
    if let Ok(child) = root.lookup(fname) {
        let _ = root.unlink(fname, &*child);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// L8: 元数据操作
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "bench")]
fn run_fs_meta(tag: &str, sb: &Arc<Superblock>) {
    let root = &sb.root_inode;
    let cred = Credentials::root();

    let t0 = hal::time::monotonic_ns();
    let dir = match root.open_ops(&OpenOptions::default(), &cred) {
        Ok(d) => d,
        Err(e) => {
            log::error!("[bench][{}][L8-meta] open root failed: {:?}", tag, e);
            return;
        }
    };
    let mut count = 0u32;
    let _ = dir.readdir(0, &mut |_| {
        count += 1;
        ControlFlow::Continue(())
    });
    drop(dir);
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][{}][L8-meta] readdir {} entries in {} ns",
        tag,
        count,
        dt
    );

    let mut create_total = 0u64;
    let mut delete_total = 0u64;
    for i in 0..10 {
        let name = alloc::format!("_bmt{}", i);
        let t0 = hal::time::monotonic_ns();
        match root.create(&name, vfs::stat::FileMode::new(0o644), &cred) {
            Ok(inode) => {
                let dt = hal::time::monotonic_ns().saturating_sub(t0);
                create_total += dt;
                let t0 = hal::time::monotonic_ns();
                if root.unlink(&name, &*inode).is_ok() {
                    delete_total += hal::time::monotonic_ns().saturating_sub(t0);
                }
            }
            Err(e) => {
                log::error!("[bench][{}][L8-meta] error: {:?}", tag, e);
            }
        }
    }
    if create_total > 0 {
        log::info!(
            "[bench][{}][L8-meta] 10x create: avg {} ns  10x delete: avg {} ns",
            tag,
            create_total / 10,
            delete_total / 10
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// RAM 块驱动（带 read_sectors_sync 快速路径）
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "bench")]
struct RamBlockIo {
    data: Spinlock<Vec<u8>>,
}

#[cfg(feature = "bench")]
impl RamBlockIo {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data: Spinlock::new(data),
        }
    }
}

#[cfg(feature = "bench")]
impl BlockDriver for RamBlockIo {
    fn queue_bio(&self, mut bio: Bio) -> Result<(), (SubmitError, Bio)> {
        const LBS: usize = 512;
        let off = bio.range.lba as usize * LBS;
        let want = bio.range.blocks as usize * LBS;

        match bio.op {
            BioOp::Read => {
                let data = self.data.lock();
                if off + want > data.len() {
                    drop(data);
                    bio.complete(Err(BioIoError::MediaError));
                    return Ok(());
                }
                bio.buffer.as_mut_slice()[..want].copy_from_slice(&data[off..off + want]);
                drop(data);
                bio.complete(Ok(()));
            }
            BioOp::Write => {
                let mut data = self.data.lock();
                if off + want > data.len() {
                    drop(data);
                    bio.complete(Err(BioIoError::MediaError));
                    return Ok(());
                }
                data[off..off + want].copy_from_slice(&bio.buffer.as_slice()[..want]);
                drop(data);
                bio.complete(Ok(()));
            }
            BioOp::Flush => {
                bio.complete(Ok(()));
            }
            _ => {
                bio.complete(Err(BioIoError::Unsupported));
            }
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
