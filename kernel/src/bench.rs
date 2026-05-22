//! 启动期 fatfs / extfs 挂载测试 + 性能 bench。
//!
//! 在 [`main()`] 里调用;要求 `DEVICES.block_devs` 里已经有至少两台 virtio-blk。
//! 本模块不涉及调度器/用户态,所有 I/O 都在调用上下文直接同步执行。
//!
//! 设备分配约定:
//! - `virtio-blk0` → 挂 fatfs(读写)
//! - `virtio-blk1` → 挂 extfs(只读语义测试 + 可选写)
//!
//! 如果没找到对应设备,打印警告并跳过对应那组 bench。不会 panic。

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::ControlFlow;

use vfs::cred::Credentials;
use vfs::file::{DirEntry, OpenOptions};
use vfs::superblock::{FsDriver, Superblock};

use general::dev::block::BlockDevice;
use general::dev::block_sync::SyncBlockBackend;
use general::dev::enumerate::DEVICES;

// ── 入口 ────────────────────────────────────────────────────────────────

/// 执行完整挂载测试 + 性能 bench。
///
/// 打印全部关键里程碑。不会 panic,即使某一组失败也会继续下一组。
pub fn run() {
    log::info!("[bench] =========================================");
    log::info!("[bench] filesystem mount & performance test start");
    log::info!("[bench] =========================================");

    let devs = list_block_devices();
    if devs.is_empty() {
        log::warning!("[bench] no virtio-blk devices registered; skipping");
        return;
    }
    log::info!("[bench] found {} block device(s)", devs.len());
    for (i, d) in devs.iter().enumerate() {
        let g = d.geometry();
        log::info!(
            "[bench]   [{}] name={} lbs={} pbs={} blocks={:?}",
            i,
            d.name(),
            g.logical_block_size().get(),
            g.physical_block_size().get(),
            g.block_count(),
        );
    }

    if let Some(dev) = devs.get(0) {
        run_fatfs_bench(Arc::clone(dev));
    } else {
        log::warning!("[bench] no virtio-blk0 for fatfs; skipping");
    }
    if let Some(dev) = devs.get(1) {
        run_extfs_bench(Arc::clone(dev));
    } else {
        log::warning!("[bench] no virtio-blk1 for extfs; skipping");
    }

    log::info!("[bench] =========================================");
    log::info!("[bench] filesystem tests finished");
    log::info!("[bench] =========================================");
}

fn list_block_devices() -> Vec<Arc<BlockDevice>> {
    DEVICES.block_devs.list().unwrap_or_default()
}

// ── FAT32 ───────────────────────────────────────────────────────────────

fn run_fatfs_bench(dev: Arc<BlockDevice>) {
    log::info!("[bench][fat] --- fat32 on {} ---", dev.name());
    let backend = Arc::new(SyncBlockBackend::new(Arc::clone(&dev)));
    let driver = alloc::boxed::Box::leak(alloc::boxed::Box::new(fatfs::FatFsDriver::new()));
    driver.bind_backend(backend);

    let sb = match driver.mount(None, "") {
        Ok(sb) => {
            log::info!(
                "[bench][fat] mount OK: fs_type={} block_size={}",
                sb.fs_type,
                sb.block_size
            );
            sb
        }
        Err(e) => {
            log::error!("[bench][fat] mount FAILED: {:?}", e);
            return;
        }
    };

    run_generic_bench("fat", &sb);
}

// ── ext4 ────────────────────────────────────────────────────────────────

fn run_extfs_bench(dev: Arc<BlockDevice>) {
    log::info!("[bench][ext] --- extfs on {} ---", dev.name());
    let backend = Arc::new(SyncBlockBackend::new(Arc::clone(&dev)));
    let driver = alloc::boxed::Box::leak(alloc::boxed::Box::new(extfs::ExtFsDriver::new()));
    driver.bind_backend(backend);

    let sb = match driver.mount(None, "") {
        Ok(sb) => {
            log::info!(
                "[bench][ext] mount OK: fs_type={} block_size={}",
                sb.fs_type,
                sb.block_size
            );
            sb
        }
        Err(e) => {
            log::error!("[bench][ext] mount FAILED: {:?}", e);
            return;
        }
    };

    run_generic_bench("ext", &sb);
}

// ── 共享 bench 逻辑 ─────────────────────────────────────────────────────

fn run_generic_bench(tag: &str, sb: &Arc<Superblock>) {
    // 1) 遍历根目录,找一个最大的普通文件作为读测试目标
    let root = &sb.root_inode;
    let cred = Credentials::root();

    let opts = OpenOptions::default();
    let dir = match root.open_ops(&opts, &cred) {
        Ok(d) => d,
        Err(e) => {
            log::error!("[bench][{}] open(root) failed: {:?}", tag, e);
            return;
        }
    };

    let mut entries: Vec<DirEntry> = Vec::new();
    let _ = dir.readdir(0, &mut |e: DirEntry| {
        entries.push(e);
        ControlFlow::Continue(())
    });
    log::info!("[bench][{}] root has {} entries", tag, entries.len());
    for e in entries.iter().take(16) {
        log::info!(
            "[bench][{}]   ino={} kind={:?} name={}",
            tag,
            e.ino,
            e.kind,
            core::str::from_utf8(e.name.as_bytes()).unwrap_or("<utf8?>")
        );
    }

    let mut target_name: Option<alloc::string::String> = None;
    let mut largest: u64 = 0;
    for e in &entries {
        if matches!(e.kind, vfs::stat::FileType::Regular) {
            let name_s = core::str::from_utf8(e.name.as_bytes()).unwrap_or("");
            if name_s == "." || name_s == ".." || name_s.is_empty() {
                continue;
            }
            if let Ok(child) = root.lookup(name_s) {
                let sz = child.size();
                if sz > largest {
                    largest = sz;
                    target_name = Some(name_s.into());
                }
            }
        }
    }

    let Some(target_name) = target_name else {
        log::warning!("[bench][{}] no regular file in root; skipping read bench", tag);
        return;
    };

    let target = match root.lookup(&target_name) {
        Ok(t) => t,
        Err(e) => {
            log::error!("[bench][{}] lookup({}) failed: {:?}", tag, target_name, e);
            return;
        }
    };
    let size = target.size();
    log::info!(
        "[bench][{}] target file '{}' size={} bytes",
        tag,
        target_name,
        size
    );

    let file = match target.open_ops(&opts, &cred) {
        Ok(f) => f,
        Err(e) => {
            log::error!("[bench][{}] open('{}') failed: {:?}", tag, target_name, e);
            return;
        }
    };

    // 顺序读 1 MiB(或全部,取较小)
    let want = core::cmp::min(size, 1 << 20) as usize;
    if want == 0 {
        log::warning!("[bench][{}] target file is empty; skipping I/O bench", tag);
        return;
    }
    let mut buf = vec![0u8; want];
    let t0 = approx_cycles();
    match file.read_at(&mut buf, 0) {
        Ok(got) => {
            let dt_ns = approx_cycles().wrapping_sub(t0);
            let mib_per_s = if dt_ns > 0 {
                (got as u64) * 1_000_000_000 / dt_ns / (1 << 20).max(1)
            } else {
                0
            };
            log::info!(
                "[bench][{}][seq-read] {} bytes in {} ns ({} MiB/s, checksum 0x{:08x})",
                tag,
                got,
                dt_ns,
                mib_per_s,
                fletcher32(&buf[..got])
            );
        }
        Err(e) => {
            log::error!("[bench][{}][seq-read] failed: {:?}", tag, e);
            return;
        }
    }

    // 随机 4 KiB 读 64 次(LCG 选 offset,对齐到 4 KiB)
    let iters = 64u32;
    let block = 4096usize;
    if size >= block as u64 {
        let mut rng: u32 = 0xdeadbeef;
        let mut sbuf = vec![0u8; block];
        let mut total_bytes = 0usize;
        let t0 = approx_cycles();
        for _ in 0..iters {
            rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
            let max_off = (size as usize).saturating_sub(block);
            let off = (rng as usize) % (max_off / block + 1) * block;
            match file.read_at(&mut sbuf, off as u64) {
                Ok(n) => total_bytes += n,
                Err(e) => {
                    log::error!("[bench][{}][rand-read] iter failed: {:?}", tag, e);
                    break;
                }
            }
        }
        let dt_ns = approx_cycles().wrapping_sub(t0);
        let per_iter = if iters > 0 { dt_ns / iters as u64 } else { 0 };
    log::info!(
        "[bench][{}][rand-read] {} x 4KiB -> {} bytes in {} ns (avg {} ns/iter)",
        tag,
        iters,
        total_bytes,
        dt_ns,
        per_iter
    );
    }

    // readdir 遍历若干次(metadata bench)
    let mrounds = 8u32;
    let t0 = approx_cycles();
    let mut total_entries = 0u32;
    for _ in 0..mrounds {
        let dir_f = match root.open_ops(&opts, &cred) {
            Ok(d) => d,
            Err(_) => break,
        };
        let mut count = 0u32;
        let _ = dir_f.readdir(0, &mut |_e: DirEntry| {
            count += 1;
            ControlFlow::Continue(())
        });
        total_entries += count;
        drop(dir_f);
    }
    let dt_ns = approx_cycles().wrapping_sub(t0);
    log::info!(
        "[bench][{}][metadata] {}x readdir = {} entries in {} ns",
        tag,
        mrounds,
        total_entries,
        dt_ns
    );
}

// ── 时间戳(ns) ─────────────────────────────────────────────────────────

#[inline(never)]
fn approx_cycles() -> u64 {
    // 用 loongarch64 的稳定计数器(arch::kernel_timestamp_ns 换算为 ns)
    // 这是真实时间,分辨率取决于 STABLE_TIMER_HZ。
    arch::kernel_timestamp_ns()
}

fn fletcher32(data: &[u8]) -> u32 {
    let mut a: u32 = 0;
    let mut b: u32 = 0;
    for &x in data {
        a = (a + x as u32) % 65535;
        b = (b + a) % 65535;
    }
    (b << 16) | a
}
