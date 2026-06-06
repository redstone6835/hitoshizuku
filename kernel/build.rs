//! 内核构建脚本：在 cargo 编译前交叉编译 busybox、lua，并打包 initramfs。
//!
//! 每步检查产物是否存在以支持增量构建。通过 `TARGET` 环境变量自动选择架构。

use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Arch {
    La,
    Rv,
}

impl Arch {
    fn from_target(target: &str) -> Self {
        if target.starts_with("loongarch64") {
            Arch::La
        } else if target.starts_with("riscv64") {
            Arch::Rv
        } else {
            panic!("unsupported TARGET: {target}");
        }
    }

    fn rootfs_dir(self) -> &'static str {
        match self {
            Arch::La => "userland/rootfs-la",
            Arch::Rv => "userland/rootfs-rv",
        }
    }

    fn cross_prefix(self) -> &'static str {
        match self {
            Arch::La => "loongarch64-linux-gnu-",
            Arch::Rv => "riscv64-linux-musl-",
        }
    }
}

fn main() {
    let target = std::env::var("TARGET").expect("TARGET not set by cargo");
    let arch = Arch::from_target(&target);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");

    build_busybox(&root, arch);
    build_lua(&root, arch);
    pack_initramfs(&root, arch);

    println!(
        "cargo:rerun-if-changed={}",
        root.join(arch.rootfs_dir()).display()
    );
}

// ── busybox ─────────────────────────────────────────────────────────────────────

fn build_busybox(root: &Path, arch: Arch) {
    let dest_bin = root.join(arch.rootfs_dir()).join("bin/busybox");
    if dest_bin.exists() {
        return;
    }

    let src = root.join("third/busybox-1.36.1");
    let cross = format!("CROSS_COMPILE={}", arch.cross_prefix());

    // defconfig
    let config = src.join(".config");
    if !config.exists() {
        run_cmd(
            Command::new("make")
                .arg("-C").arg(&src).arg(&cross)
                .arg("defconfig"),
        );
    }

    // 配置 CONFIG_STATIC=y, CONFIG_PIE=y, CONFIG_TC=n
    run_cmd(Command::new("sh").arg("-c").arg(&format!(
        "sed -i 's/.*CONFIG_STATIC.*/CONFIG_STATIC=y/' {} && \
         sed -i 's/.*CONFIG_PIE.*/CONFIG_PIE=y/' {} && \
         sed -i 's/^CONFIG_TC=.*/# CONFIG_TC is not set/' {}",
        config.display(), config.display(), config.display()
    )));

    // 非交互式 oldconfig
    run_cmd(Command::new("sh").arg("-c").arg(&format!(
        "yes '' | make -C {} {} oldconfig",
        src.display(), cross
    )));

    // 编译
    let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    run_cmd(
        Command::new("make")
            .arg("-C").arg(&src).arg(&cross)
            .arg(format!("-j{jobs}")),
    );

    // 安装
    let dest = root.join(arch.rootfs_dir());
    let _ = std::fs::create_dir_all(&dest);
    run_cmd(
        Command::new("make")
            .arg("-C").arg(&src).arg(&cross)
            .arg(format!("CONFIG_PREFIX={}", dest.display()))
            .arg("install"),
    );

    // strip
    let _ = Command::new(format!("{}strip", arch.cross_prefix()))
        .arg(&dest_bin)
        .status();

    // distclean
    run_cmd(Command::new("make").arg("-C").arg(&src).arg("distclean"));
}

// ── lua ─────────────────────────────────────────────────────────────────────────

fn build_lua(root: &Path, arch: Arch) {
    let dest_bin = root.join(arch.rootfs_dir()).join("bin/lua");
    if dest_bin.exists() {
        return;
    }

    let src = root.join("third/lua");
    let cc = format!("CC={}gcc", arch.cross_prefix());

    let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    run_cmd(
        Command::new("make")
            .arg("-C").arg(&src)
            .arg("all")
            .arg(&cc)
            .arg("MYCFLAGS=-std=c99 -static -fPIE -DLUA_USE_POSIX")
            .arg("MYLDFLAGS=-static")
            .arg("MYLIBS=-lm")
            .arg(format!("-j{jobs}")),
    );

    let dest = root.join(arch.rootfs_dir()).join("bin");
    let _ = std::fs::create_dir_all(&dest);
    std::fs::copy(src.join("lua"), &dest_bin).unwrap();

    let _ = Command::new(format!("{}strip", arch.cross_prefix()))
        .arg(&dest_bin)
        .status();

    let _ = Command::new("make").arg("-C").arg(&src).arg("clean").status();
}

// ── initramfs ───────────────────────────────────────────────────────────────────

fn pack_initramfs(root: &Path, arch: Arch) {
    let out_cpio = root.join("build/initramfs.cpio");
    let src = root.join(arch.rootfs_dir());

    // cpio 比 rootfs 目录新则跳过
    if out_cpio.exists() {
        if let Ok(cpio_time) = std::fs::metadata(&out_cpio).and_then(|m| m.modified()) {
            let mut newer_than_cpio = false;
            check_modified(&src, cpio_time, &mut newer_than_cpio);
            if !newer_than_cpio {
                return;
            }
        }
    }

    let _ = std::fs::create_dir_all(out_cpio.parent().unwrap());
    run_cmd(Command::new("sh").arg("-c").arg(&format!(
        "cd {} && find . -print0 | cpio --quiet -o -0 -H newc > {}",
        src.display(),
        out_cpio.display()
    )));
}

fn check_modified(dir: &Path, ref_time: std::time::SystemTime, result: &mut bool) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if *result {
            return;
        }
        if let Ok(meta) = entry.metadata() {
            if meta.modified().map_or(false, |t| t > ref_time) {
                *result = true;
                return;
            }
            if meta.is_dir() {
                check_modified(&entry.path(), ref_time, result);
            }
        }
    }
}

// ── helper ──────────────────────────────────────────────────────────────────────

fn run_cmd(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {cmd:?}: {e}"));
    assert!(status.success(), "{cmd:?} exited with {status:?}");
}
