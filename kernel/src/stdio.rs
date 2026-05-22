//! 给 init 任务预装 fd 0/1/2 指向控制台。
//!
//! 用已经挂到 devtmpfs 下的 console 字符设备节点（优先 `/dev/console`，
//! 字符设备节点（优先 `/dev/console`，也兼容 `/dev/uart0`）走 VFS 正式
//! `openat`，拿到 `Arc<File>`
//! 之后 `FdTable::install_fd` 三次装 fd 0/1/2。这样 `sys_write(1, ...)` 能
//! 正常经过 FdTable → File → CharDevFileOps → console_write 链路。
//!
//! 启动期 `kernel/src/{acpi,dtb}.rs` 已经调用
//! [`crate::sched::stash_boot_console_name`] 存好 console 在 `/dev` 下的节
//! 点名或绝对路径，[`install_stdio`] 据此打开它。

use alloc::string::String;
use alloc::sync::Arc;

use vfs::VfsContext;
use vfs::fdtable::{Fd, FdFlags, FdTable};
use vfs::file::{AccessMode, OpenOptions};
use vfs::operation;
use vfs::path::Dirfd;
use vfs::stat::FileMode;

/// 给 `fdt` 装 fd 0/1/2 指向控制台路径。
///
/// 失败不 panic——控制台没挂时 init 依然能跑（它现在也只做调度器 smoketest
/// 之类内核内活动）。仅用 log 留痕。
pub fn install_stdio(vfs_ctx: &VfsContext, fdt: &FdTable, console_path_or_name: &str) {
    let path = if console_path_or_name.starts_with('/') {
        String::from(console_path_or_name)
    } else {
        alloc::format!("/dev/{}", console_path_or_name)
    };
    let flags = OpenOptions {
        access: AccessMode::ReadWrite,
        ..Default::default()
    };
    let fd_flags = FdFlags::default();

    // 先 openat 拿到一个 File；随后显式安装到 fd 0/1/2。不要假设 openat
    // 必然返回 0：启动期自检或后续扩展可能已经占用了低 fd。
    let fd0 = match operation::openat(vfs_ctx, fdt, &Dirfd::Cwd, &path, flags, FileMode::new(0)) {
        Ok(fd) => fd,
        Err(e) => {
            log::info!(
                "[stdio] openat {:?} failed: {:?}; leaving fd 0/1/2 empty",
                path,
                e
            );
            return;
        }
    };
    let Some(file) = fdt.get_file(fd0) else {
        log::info!("[stdio] fd {} missing after openat; abort", fd0.as_raw());
        return;
    };

    let ok0 = install_at(fdt, Fd::STDIN, Arc::clone(&file), fd_flags, &path);
    let ok1 = install_at(fdt, Fd::STDOUT, Arc::clone(&file), fd_flags, &path);
    let ok2 = install_at(fdt, Fd::STDERR, file, fd_flags, &path);

    if fd0.as_raw() > Fd::STDERR.as_raw() {
        if let Err(e) = fdt.close_fd(fd0) {
            log::info!(
                "[stdio] close temporary fd {} for {} failed: {:?}",
                fd0.as_raw(),
                path,
                e
            );
        }
    }

    if !(ok0 && ok1 && ok2) {
        log::info!("[stdio] incomplete fd 0/1/2 install for {}", path);
        return;
    }
    log::info!("[stdio] installed fd 0/1/2 → {}", path);
}

fn install_at(
    fdt: &FdTable,
    fd: Fd,
    file: Arc<vfs::file::File>,
    flags: FdFlags,
    path: &str,
) -> bool {
    match fdt.install_fd(fd, file, flags) {
        Ok(()) => true,
        Err(e) => {
            log::info!(
                "[stdio] install_fd({}) for {} failed: {:?}",
                fd.as_raw(),
                path,
                e
            );
            false
        }
    }
}

/// 便利包装：给定 console user_name（可能是 `String`），把 stdio 装到 init 的
/// FdTable 上。在 `sched::boot_init` 挂完 FdTable 之后调。
pub fn install_from_stash(vfs_ctx: &VfsContext, fdt: &FdTable, name: Option<String>) {
    let Some(name) = name else {
        log::info!("[stdio] no console stashed; fd 0/1/2 not installed");
        return;
    };
    install_stdio(vfs_ctx, fdt, &name);
}
