//! 内核构建脚本：校验并打包当前目标架构的 initramfs。
//!
//! 用户态 rootfs 的生成由顶层 Makefile 编排；这里仅把 Cargo 当前目标需要的
//! rootfs 打包为架构专属 CPIO，并把路径暴露给内核源码的 `include_bytes!`。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").expect("TARGET not set by cargo");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");

    let initramfs_root = env_path("INITRAMFS_ROOT", &root).unwrap_or_else(|| {
        root.join(default_rootfs_dir(&target).unwrap_or_else(|| {
            panic!("INITRAMFS_ROOT is required for unsupported TARGET: {target}")
        }))
    });
    let initramfs_cpio = env_path("INITRAMFS_CPIO", &root).unwrap_or_else(|| {
        root.join(default_initramfs_cpio(&target).unwrap_or_else(|| {
            panic!("INITRAMFS_CPIO is required for unsupported TARGET: {target}")
        }))
    });

    pack_initramfs(&initramfs_root, &initramfs_cpio);

    println!("cargo:rerun-if-env-changed=INITRAMFS_ROOT");
    println!("cargo:rerun-if-env-changed=INITRAMFS_CPIO");
    println!("cargo:rerun-if-changed={}", initramfs_root.display());
    println!(
        "cargo:rustc-env=MYGO_INITRAMFS_CPIO={}",
        initramfs_cpio.display()
    );
}

fn env_path(name: &str, root: &Path) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    let path = PathBuf::from(value);
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn default_rootfs_dir(target: &str) -> Option<&'static str> {
    if target.starts_with("loongarch64") {
        Some("userland/rootfs-la")
    } else if target.starts_with("riscv64") {
        Some("userland/rootfs-rv")
    } else {
        None
    }
}

fn default_initramfs_cpio(target: &str) -> Option<&'static str> {
    if target.starts_with("loongarch64") {
        Some("build/initramfs-la.cpio")
    } else if target.starts_with("riscv64") {
        Some("build/initramfs-rv.cpio")
    } else {
        None
    }
}

fn pack_initramfs(src: &Path, out_cpio: &Path) {
    if !src.is_dir() {
        panic!("initramfs root does not exist or is not a directory: {src:?}");
    }
    let parent = out_cpio
        .parent()
        .unwrap_or_else(|| panic!("initramfs output has no parent: {out_cpio:?}"));
    std::fs::create_dir_all(parent)
        .unwrap_or_else(|err| panic!("failed to create initramfs output dir {parent:?}: {err}"));

    run_cmd(Command::new("sh").arg("-c").arg(&format!(
        "cd {} && find . -print0 | cpio --quiet -o -0 -H newc > {}",
        shell_quote(src),
        shell_quote(out_cpio)
    )));
}

fn shell_quote(path: &Path) -> String {
    let raw = path.as_os_str().to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

fn run_cmd(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {cmd:?}: {e}"));
    assert!(status.success(), "{cmd:?} exited with {status:?}");
}
