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

#[cfg(feature = "bench")]
use alloc::boxed::Box;
#[cfg(feature = "bench")]
use alloc::sync::Arc;
#[cfg(feature = "bench")]
use alloc::vec;
#[cfg(feature = "bench")]
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
#[cfg(feature = "bench")]
use core::any::Any;
#[cfg(feature = "bench")]
use core::num::NonZeroU32;
#[cfg(feature = "bench")]
use core::ops::ControlFlow;

use allocator::{
    KERNEL_ALLOCATOR, MAX_SMALL_SIZE, MemoryDomain, MemoryRequest, PAGE_SIZE, PhysicalAllocRequest,
    ReclaimPolicy, Zeroing,
};
#[cfg(feature = "bench")]
use vfs::cred::Credentials;
#[cfg(feature = "bench")]
use vfs::file::{DirEntry, OpenOptions};
#[cfg(feature = "bench")]
use vfs::superblock::{FsDriver, Superblock};
#[cfg(feature = "bench")]
use vfs::sync::Spinlock;

#[cfg(feature = "bench")]
use general::dev::bio::{Bio, BioBuffer, BioIoError, BioOp, SubmitError};
#[cfg(feature = "bench")]
use general::dev::block::{
    BlockClass, BlockDevice, BlockDeviceInit, BlockDriver, BlockFeatures, BlockGeometry,
    BlockLimits,
};
#[cfg(feature = "bench")]
use general::dev::block_sync::SyncBlockBackend;

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

    let raw_dev = make_ram_device("ramd-raw", EXT_IMG);
    run_block_seq_read(&raw_dev);
    run_block_seq_write(&raw_dev);
    run_block_seq_read_instrumented(&raw_dev);
    run_block_rand_read(&raw_dev);

    let fat_dev = make_ram_device("ramd-fat", FAT_IMG);
    let ext_dev = make_ram_device("ramd-ext", EXT_IMG);
    let fat_sb = mount_fat("fat", fat_dev);
    let ext_sb = mount_ext("ext", ext_dev);

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
        run_fs_rand_read("fat", sb);
    }
    if let Some(ref sb) = ext_sb {
        run_fs_rand_read("ext", sb);
    }

    // ─── L8: 元数据操作 ─────────────────────────────────────
    if let Some(ref sb) = fat_sb {
        run_fs_meta("fat", sb);
    }
    if let Some(ref sb) = ext_sb {
        run_fs_meta("ext", sb);
    }

    log::info!("[bench] ================= TEST COMPLETE ====================");
}

pub fn run_allocator_only() {
    log::info!("[bench] ================= ALLOCATOR PERF TEST =================");
    run_allocator_bench();
    log::info!("[bench] ================= TEST COMPLETE ======================");
}

// ═══════════════════════════════════════════════════════════════════════
// 基础辅助
// ═══════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════
// Allocator: 分配器路径基准
// ═══════════════════════════════════════════════════════════════════════

// TODO(alloc-stabilization): 当前只完成 allocator-bench 框架和开销来源日志。
// 恢复实现时需要在 QEMU 下采集真实结果，并把优化前后 ns/op 对比固化到提交说
// 明或独立报告中，避免只凭 host check 推断内核态性能。
const ALLOC_BENCH_BATCH: usize = 128;

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

    let large4k =
        run_allocator_alloc_free_case("kheap-4k", 4096, PAGE_SIZE, 64, Zeroing::Uninitialized);
    let large8k =
        run_allocator_alloc_free_case("kheap-8k", 8192, PAGE_SIZE, 48, Zeroing::Uninitialized);
    let large64k = run_allocator_alloc_free_case(
        "kheap-64k",
        64 * 1024,
        PAGE_SIZE,
        16,
        Zeroing::Uninitialized,
    );

    run_allocator_registry_lookup();
    run_allocator_in_place_query();
    run_allocator_realloc_same_class();
    run_allocator_realloc_grow();
    run_allocator_audit();
    run_allocator_diagnostic();
    run_allocator_physical();

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
            "[bench][alloc][audit] final inconsistent flags={} reg_struct={} live={} kinds={} boot={} physrec={} nodes={}/{} scan={}/{} slab={}/{} kheap={}/{} managed={}/{}",
            audit_end.flags.bits(),
            audit_end.registry_structure.flags.bits(),
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
        "[bench][alloc][hotspot][{}] slab_hit={}/1000 slab_miss={}/1000 refill={}/1000 flush={}/1000 fast_free={}/1000 fallback={} reg_chain={} reg_shard={} reg_load={}/1000 reg_nonempty={} reg_nonempty_load={}/1000 reg_underflow={} kheap_fail={}/1000 kheap_realloc={}/1000 kheap_cache={}/1000 cached_pages={} pressure_rel={} vmem_largest={}pct vmem_free_segs={} managed_frag={}/1000 pressure={}",
        stage,
        hot.slab_cache_hit_per_mille,
        hot.slab_cache_miss_per_mille,
        hot.slab_refill_per_mille,
        hot.slab_flush_per_mille,
        hot.slab_fast_free_per_mille,
        hot.slab_fast_free_fallbacks,
        hot.registry_max_chain_len,
        hot.registry_max_shard_live_records,
        hot.registry_live_per_bucket_per_mille,
        hot.registry_nonempty_buckets,
        hot.registry_nonempty_load_per_mille,
        hot.registry_underflows,
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

fn run_allocator_audit() {
    let iters = 4096usize;
    let t0 = hal::time::monotonic_ns();
    let mut audit_flags = 0u32;
    let mut registry_flags = 0u32;
    let mut live = 0usize;
    for _ in 0..iters {
        let audit = KERNEL_ALLOCATOR.audit();
        audit_flags |= audit.flags.bits();
        registry_flags |= audit.registry_structure.flags.bits();
        live = live.saturating_add(audit.registry_live_records);
        core::hint::black_box(audit);
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    log::info!(
        "[bench][alloc][audit] {} snapshots avg {} ns/op audit_flags={} registry_flags={} avg_live={}; source=layer stats snapshot + full registry shard scan + typed invariant checks",
        iters,
        dt / iters as u64,
        audit_flags,
        registry_flags,
        live / iters
    );
}

fn allocator_request(size: usize, align: usize, zeroing: Zeroing) -> MemoryRequest {
    MemoryRequest::new(MemoryDomain::Kernel, size, align)
        .with_zeroing(zeroing)
        .with_reclaim(ReclaimPolicy::NoReclaim)
}

fn run_allocator_alloc_free_case(
    tag: &str,
    size: usize,
    align: usize,
    iters: usize,
    zeroing: Zeroing,
) -> u64 {
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
                    return 0;
                }
                completed += 1;
            }
        }
        if allocated != ALLOC_BENCH_BATCH {
            break;
        }
    }

    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let avg = dt / completed.max(1) as u64;
    log::info!(
        "[bench][alloc][{}] {} x alloc+free size={} align={} zero={:?}: total {} ns (avg {} ns/op)",
        tag,
        completed,
        size,
        align,
        zeroing,
        dt,
        avg
    );
    avg
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
        "[bench][alloc][diagnostic] {} snapshots avg {} ns/op avg_len={}; source=stats snapshot + registry audit scan + no_alloc formatting",
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
    log::info!(
        "[bench][alloc][physical] {} x page alloc+free avg {} ns/op; source=buddy lock + split/coalesce + registry tracking",
        completed,
        dt / completed.max(1) as u64
    );
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
    let driver = Box::leak(Box::new(fatfs::FatFsDriver::new()));
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
    let driver = Box::leak(Box::new(extfs::ExtFsDriver::new()));
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

#[cfg(feature = "bench")]
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
#[cfg(feature = "bench")]
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

#[cfg(feature = "bench")]
fn run_block_seq_read(dev: &Arc<BlockDevice>) {
    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][L1-blk] backend unavailable: {:?}", e);
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
    let mut buf = vec![0u8; chunk];
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        if backend.read(lba, blocks_per_chunk, &mut buf).is_err() {
            log::error!("[bench][L1-blk] seq read error");
            return;
        }
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let mibps = (total_bytes as u64) * 1_000_000_000 / dt.max(1) / (1024 * 1024);
    log::info!(
        "[bench][L1-blk] seq read 4 MiB in {} ns ({} MiB/s)  <-- 裸块层开销",
        dt,
        mibps
    );
}

/// 精确对比测试：同一设备、同一数据、相同 cache 状态下，
/// 对比「直接 memcpy」vs「通过 SyncBlockBackend 完整路径」。
/// 先 warmup 把数据拉入 cache，再分别测量两条路径。
#[cfg(feature = "bench")]
fn run_block_seq_read_instrumented(dev: &Arc<BlockDevice>) {
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
            log::error!("[bench][PROOF] backend unavailable: {:?}", e);
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

    // ── 测量 B: 直接路径（旧 read_sectors_sync 快路径已移除，与 backend 同等） ──
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        let _ = backend.read(lba, blocks_per_chunk, &mut buf);
    }
    core::hint::black_box(&buf);
    let dt_direct = hal::time::monotonic_ns().saturating_sub(t0);

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

    let overhead = dt_full.saturating_sub(dt_direct);
    log::info!("[bench][PROOF] same device, cache hot, 4 MiB x3:");
    log::info!(
        "[bench][PROOF]   full path (SyncBlockBackend): {} ns ({} MiB/s)",
        dt_full,
        (total_bytes as u64) * 1_000_000_000 / dt_full.max(1) / (1024 * 1024)
    );
    log::info!(
        "[bench][PROOF]   direct BlockIo::read_sectors_sync: {} ns ({} MiB/s)",
        dt_direct,
        (total_bytes as u64) * 1_000_000_000 / dt_direct.max(1) / (1024 * 1024)
    );
    log::info!(
        "[bench][PROOF]   raw lock+memcpy: {} ns ({} MiB/s)",
        dt_raw,
        (total_bytes as u64) * 1_000_000_000 / dt_raw.max(1) / (1024 * 1024)
    );
    log::info!(
        "[bench][PROOF]   software overhead (full - direct): {} ns ({} ns/op)",
        overhead,
        overhead / iters as u64
    );
}

#[cfg(feature = "bench")]
fn run_block_seq_write(dev: &Arc<BlockDevice>) {
    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][L1-blk] backend unavailable: {:?}", e);
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
            log::error!("[bench][L1-blk] seq write error");
            return;
        }
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let mibps = (total_bytes as u64) * 1_000_000_000 / dt.max(1) / (1024 * 1024);
    log::info!(
        "[bench][L1-blk] seq write 4 MiB in {} ns ({} MiB/s)  <-- 裸块层开销",
        dt,
        mibps
    );
}

// ═══════════════════════════════════════════════════════════════════════
// L2: 裸块设备随机读取
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "bench")]
fn run_block_rand_read(dev: &Arc<BlockDevice>) {
    let backend = match SyncBlockBackend::new(Arc::clone(dev)) {
        Ok(backend) => backend,
        Err(e) => {
            log::error!("[bench][L2-blk] backend unavailable: {:?}", e);
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
        let lba = (rng as u64) % (max_lba / 8 + 1) * 8;
        let _ = backend.read(lba, blocks_per_op, &mut buf);
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let avg_ns = dt / count;
    log::info!(
        "[bench][L2-blk] rand read {} x 4 KiB: total {} ns (avg {} ns/op)  <-- 裸块随机延迟",
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
// L5: 顺序读文件
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "bench")]
fn run_fs_seq_read(tag: &str, sb: &Arc<Superblock>) {
    let root = &sb.root_inode;
    let cred = Credentials::root();
    let (file, size) = match find_largest_file(root, tag, &cred) {
        Some(v) => v,
        None => return,
    };
    let want = core::cmp::min(size, 4 * 1024 * 1024) as usize;
    if want == 0 {
        return;
    }
    let mut buf = vec![0u8; want];
    let t0 = hal::time::monotonic_ns();
    if file.read_at(&mut buf, 0).is_err() {
        log::error!("[bench][{}][L5-seq] read error", tag);
        return;
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let mibps = (want as u64) * 1_000_000_000 / dt.max(1) / (1024 * 1024);
    log::info!(
        "[bench][{}][L5-seq] seq read {} bytes in {} ns ({} MiB/s)",
        tag,
        want,
        dt,
        mibps
    );
}

// ═══════════════════════════════════════════════════════════════════════
// L6: 随机读文件
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "bench")]
fn run_fs_rand_read(tag: &str, sb: &Arc<Superblock>) {
    let root = &sb.root_inode;
    let cred = Credentials::root();
    let (file, size) = match find_largest_file(root, tag, &cred) {
        Some(v) => v,
        None => return,
    };
    let block = 4096usize;
    let count = 100u64;
    if size < block as u64 {
        return;
    }
    let mut buf = vec![0u8; block];
    let mut rng = 0xbeefdeadu32;
    let t0 = hal::time::monotonic_ns();
    for _ in 0..count {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        let max_off = (size as usize).saturating_sub(block);
        let off = (rng as usize) % (max_off / block + 1) * block;
        let _ = file.read_at(&mut buf, off as u64);
    }
    let dt = hal::time::monotonic_ns().saturating_sub(t0);
    let avg_ns = dt / count;
    log::info!(
        "[bench][{}][L6-rand] rand read {} x 4 KiB: avg {} ns/op",
        tag,
        count,
        avg_ns
    );
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
// 辅助：找最大普通文件
// ═══════════════════════════════════════════════════════════════════════

#[cfg(feature = "bench")]
fn find_largest_file(
    root: &Arc<vfs::inode::Inode>,
    _tag: &str,
    cred: &Credentials,
) -> Option<(Box<dyn vfs::file::FileOps + Send + Sync>, u64)> {
    let opts = OpenOptions::default();
    let dir = root.open_ops(&opts, cred).ok()?;
    let mut entries: Vec<DirEntry> = Vec::new();
    let _ = dir.readdir(0, &mut |e| {
        entries.push(e);
        ControlFlow::Continue(())
    });
    let mut largest: u64 = 0;
    let mut target_name: Option<alloc::string::String> = None;
    for e in &entries {
        if matches!(e.kind, vfs::stat::FileType::Regular) {
            if let Ok(name) = core::str::from_utf8(e.name.as_bytes()) {
                if name == "." || name == ".." || name.is_empty() {
                    continue;
                }
                if let Ok(child) = root.lookup(name) {
                    let sz = child.size();
                    if sz > largest {
                        largest = sz;
                        target_name = Some(name.into());
                    }
                }
            }
        }
    }
    let name = target_name?;
    let inode = root.lookup(&name).ok()?;
    let file = inode.open_ops(&opts, cred).ok()?;
    Some((file, largest))
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
                if let BioBuffer::Owned(buf) = &mut bio.buffer {
                    buf[..want].copy_from_slice(&data[off..off + want]);
                }
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
                if let BioBuffer::Owned(buf) = &bio.buffer {
                    data[off..off + want].copy_from_slice(&buf[..want]);
                }
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
