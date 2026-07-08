//! 内核构建脚本：校验并打包当前目标架构的 initramfs。
//!
//! 用户态 rootfs 的生成由顶层 Makefile 编排；这里仅把 Cargo 当前目标需要的
//! rootfs 打包为架构专属 CPIO，并把路径暴露给内核源码的 `include_bytes!`。

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").expect("Cargo 未设置 TARGET 环境变量");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");

    let initramfs_root = env_path("INITRAMFS_ROOT", &root).unwrap_or_else(|| {
        root.join(
            default_rootfs_dir(&target)
                .unwrap_or_else(|| panic!("不支持的目标 {target}，必须显式设置 INITRAMFS_ROOT")),
        )
    });
    let initramfs_cpio = env_path("INITRAMFS_CPIO", &root).unwrap_or_else(|| {
        root.join(
            default_initramfs_cpio(&target)
                .unwrap_or_else(|| panic!("不支持的目标 {target}，必须显式设置 INITRAMFS_CPIO")),
        )
    });

    pack_initramfs(&initramfs_root, &initramfs_cpio);

    println!("cargo:rerun-if-env-changed=INITRAMFS_ROOT");
    println!("cargo:rerun-if-env-changed=INITRAMFS_CPIO");
    emit_initramfs_rerun_inputs(&initramfs_root);
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
        panic!("initramfs 根目录不存在或不是目录：{src:?}");
    }
    let parent = out_cpio
        .parent()
        .unwrap_or_else(|| panic!("initramfs 输出路径没有父目录：{out_cpio:?}"));
    std::fs::create_dir_all(parent)
        .unwrap_or_else(|err| panic!("创建 initramfs 输出目录 {parent:?} 失败：{err}"));

    run_cmd(Command::new("sh").arg("-c").arg(&format!(
        "cd {} && find . -print0 | cpio --quiet -o -0 -H newc > {}",
        shell_quote(src),
        shell_quote(out_cpio)
    )));
}

fn emit_initramfs_rerun_inputs(root: &Path) {
    // Cargo 对目录的 rerun-if-changed 不是递归语义。initramfs 里 rcS、
    // busybox applet 链接等任一文件变更都必须触发重新打包，否则内嵌
    // cpio 会继续使用旧内容，启动行为和源码工作区不一致。
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        println!("cargo:rerun-if-changed={}", path.display());
        if !path.is_dir() {
            continue;
        }
        let mut entries = std::fs::read_dir(&path)
            .unwrap_or_else(|err| panic!("读取 initramfs 目录 {path:?} 失败：{err}"))
            .map(|entry| {
                entry.unwrap_or_else(|err| panic!("读取 initramfs 目录项 {path:?} 失败：{err}"))
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.path());
        for entry in entries.into_iter().rev() {
            stack.push(entry.path());
        }
    }
}

fn shell_quote(path: &Path) -> String {
    let raw = path.as_os_str().to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

fn run_cmd(cmd: &mut Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("启动命令 {cmd:?} 失败：{e}"));
    assert!(status.success(), "命令 {cmd:?} 退出状态异常：{status:?}");
}
