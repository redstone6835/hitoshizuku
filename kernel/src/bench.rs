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

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::num::NonZeroU32;
use core::ops::ControlFlow;

use vfs::cred::Credentials;
use vfs::file::{DirEntry, OpenOptions};
use vfs::superblock::{FsDriver, Superblock};
use vfs::sync::Spinlock;

use general::dev::block::{
    BlockClass, BlockCompletion, BlockDevice, BlockDeviceInit, BlockDeviceKind, BlockFeatures,
    BlockGeometry, BlockIo, BlockIoCompletion, BlockIoError, BlockIoRequest, BlockLimits,
    BlockSubmitError,
};
use general::dev::block_sync::SyncBlockBackend;

// ── 嵌入的磁盘镜像 ──────────────────────────────────────────────────────

static FAT_IMG: &[u8] = include_bytes!("../../build/fat32.img");
static EXT_IMG: &[u8] = include_bytes!("../../build/ext4.img");

// ── 测试入口 ────────────────────────────────────────────────────────────

pub fn run() {
    log::info!("[bench] ================= LAYERED PERF TEST =================");

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

// ═══════════════════════════════════════════════════════════════════════
// 基础辅助
// ═══════════════════════════════════════════════════════════════════════

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
            kind: BlockDeviceKind::RamDisk,
            class: BlockClass::Whole,
            geometry: geom,
            limits: BlockLimits::unrestricted(),
            features: BlockFeatures::FLUSH,
        },
        io,
        None,
    ))
}

fn mount_fat(tag: &str, dev: Arc<BlockDevice>) -> Option<Arc<Superblock>> {
    let backend = Arc::new(SyncBlockBackend::new(Arc::clone(&dev)));
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

fn mount_ext(tag: &str, dev: Arc<BlockDevice>) -> Option<Arc<Superblock>> {
    let backend = Arc::new(SyncBlockBackend::new(Arc::clone(&dev)));
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
                kind: BlockDeviceKind::RamDisk,
                class: BlockClass::Whole,
                geometry: geom,
                limits: BlockLimits::unrestricted(),
                features: BlockFeatures::FLUSH,
            },
            io,
            None,
        ))
    };
    let backend = SyncBlockBackend::new(Arc::clone(&dev));
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

fn run_block_seq_read(dev: &Arc<BlockDevice>) {
    let backend = SyncBlockBackend::new(Arc::clone(dev));
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
fn run_block_seq_read_instrumented(dev: &Arc<BlockDevice>) {
    let chunk = 1024 * 1024usize;
    let total_bytes = 4 * 1024 * 1024usize;
    let lbs = dev.geometry().logical_block_size().get() as usize;
    if chunk % lbs != 0 {
        return;
    }
    let blocks_per_chunk = (chunk / lbs) as u32;
    let iters = total_bytes / chunk;

    let backend = SyncBlockBackend::new(Arc::clone(dev));
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

    // ── 测量 B: 直接通过 BlockIo trait 调用（绕过 SyncBlockBackend + BlockDevice） ──
    let io: &dyn BlockIo = dev.downcast_io::<RamBlockIo>().unwrap();
    let t0 = hal::time::monotonic_ns();
    for i in 0..iters {
        let lba = (i as u64) * blocks_per_chunk as u64;
        let _ = io.read_sectors_sync(lba, blocks_per_chunk, &mut buf);
    }
    core::hint::black_box(&buf);
    let dt_direct = hal::time::monotonic_ns().saturating_sub(t0);

    // ── 测量 C: 纯 memcpy（同一 backing store，手动 lock） ──
    let io_ref = dev.downcast_io::<RamBlockIo>().unwrap();
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

fn run_block_seq_write(dev: &Arc<BlockDevice>) {
    let backend = SyncBlockBackend::new(Arc::clone(dev));
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

fn run_block_rand_read(dev: &Arc<BlockDevice>) {
    let backend = SyncBlockBackend::new(Arc::clone(dev));
    let lbs = dev.geometry().logical_block_size().get() as usize;
    let block = 4096usize;
    let count = 100u64;
    if block % lbs != 0 {
        return;
    }
    let blocks_per_op = (block / lbs) as u32;
    let max_lba = dev.geometry().block_count().unwrap_or(0) - blocks_per_op as u64;
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

struct RamBlockIo {
    data: Spinlock<Vec<u8>>,
}

impl RamBlockIo {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data: Spinlock::new(data),
        }
    }
}

impl BlockIo for RamBlockIo {
    fn submit(
        &self,
        req: BlockIoRequest,
        completion: BlockCompletion,
    ) -> Result<(), (BlockSubmitError, BlockIoRequest, BlockCompletion)> {
        const LBS: usize = 512;
        let result = match &req {
            BlockIoRequest::Read { range, .. } | BlockIoRequest::Write { range, .. } => {
                let off = range.lba as usize * LBS;
                let want = range.blocks as usize * LBS;
                let data = self.data.lock();
                if off.saturating_add(want) > data.len() {
                    Err(BlockIoError::MediaError)
                } else {
                    Ok((off, want))
                }
            }
            _ => Ok((0, 0)),
        };
        let done = match (req, result) {
            (BlockIoRequest::Read { range, mut buffer }, Ok((off, want))) => {
                let data = self.data.lock();
                buffer[..want].copy_from_slice(&data[off..off + want]);
                drop(data);
                BlockIoCompletion {
                    request: BlockIoRequest::Read { range, buffer },
                    result: Ok(()),
                }
            }
            (BlockIoRequest::Write { range, buffer, fua }, Ok((off, want))) => {
                let mut data = self.data.lock();
                data[off..off + want].copy_from_slice(&buffer[..want]);
                drop(data);
                BlockIoCompletion {
                    request: BlockIoRequest::Write { range, buffer, fua },
                    result: Ok(()),
                }
            }
            (req @ BlockIoRequest::Flush, _) => BlockIoCompletion {
                request: req,
                result: Ok(()),
            },
            (req, Err(err)) => BlockIoCompletion {
                request: req,
                result: Err(err),
            },
            (req, _) => BlockIoCompletion {
                request: req,
                result: Ok(()),
            },
        };
        completion(done);
        Ok(())
    }

    fn read_sectors_sync(&self, lba: u64, count: u32, buf: &mut [u8]) -> Result<(), BlockIoError> {
        const LBS: usize = 512;
        let want = count as usize * LBS;
        if buf.len() < want {
            return Err(BlockIoError::MediaError);
        }
        let off = lba as usize * LBS;
        let data = self.data.lock();
        if off + want > data.len() {
            return Err(BlockIoError::MediaError);
        }
        buf[..want].copy_from_slice(&data[off..off + want]);
        Ok(())
    }

    fn write_sectors_sync(&self, lba: u64, count: u32, buf: &[u8]) -> Result<(), BlockIoError> {
        const LBS: usize = 512;
        let want = count as usize * LBS;
        if buf.len() < want {
            return Err(BlockIoError::MediaError);
        }
        let off = lba as usize * LBS;
        let mut data = self.data.lock();
        if off + want > data.len() {
            return Err(BlockIoError::MediaError);
        }
        data[off..off + want].copy_from_slice(&buf[..want]);
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
