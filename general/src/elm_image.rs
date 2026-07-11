//! ELM 原生镜像映射能力注入。
//!
//! ELM Core 需要在架构无关代码里请求镜像页权限切换和指令缓存同步；具体页表格式、
//! TLB/ICache 指令仍由架构层提供。这里采用函数指针注入，避免 `general` 写条件编译。

use core::sync::atomic::{AtomicUsize, Ordering};

pub type ProtectElmImageRangeFn =
    fn(vaddr: usize, size: usize, read: bool, write: bool, execute: bool) -> bool;
pub type ValidateElmImageRangeFn =
    fn(vaddr: usize, size: usize, read: bool, write: bool, execute: bool) -> bool;
pub type SyncElmImageIcacheFn = fn();

static PROTECT_FN: AtomicUsize = AtomicUsize::new(0);
static VALIDATE_FN: AtomicUsize = AtomicUsize::new(0);
static SYNC_ICACHE_FN: AtomicUsize = AtomicUsize::new(0);

pub fn register_elm_image_ops(
    protect: ProtectElmImageRangeFn,
    validate: ValidateElmImageRangeFn,
    sync_icache: SyncElmImageIcacheFn,
) {
    PROTECT_FN.store(protect as usize, Ordering::Release);
    VALIDATE_FN.store(validate as usize, Ordering::Release);
    SYNC_ICACHE_FN.store(sync_icache as usize, Ordering::Release);
}

pub fn elm_image_ops_registered() -> bool {
    PROTECT_FN.load(Ordering::Acquire) != 0
        && VALIDATE_FN.load(Ordering::Acquire) != 0
        && SYNC_ICACHE_FN.load(Ordering::Acquire) != 0
}

pub fn validate_elm_image_range(
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> bool {
    let raw = VALIDATE_FN.load(Ordering::Acquire);
    if raw == 0 {
        return false;
    }
    // 安全性：注册入口只接受函数指针，写入和读取使用 Release/Acquire 配对。
    let validate: ValidateElmImageRangeFn = unsafe { core::mem::transmute(raw) };
    validate(vaddr, size, read, write, execute)
}

pub fn protect_elm_image_range(
    vaddr: usize,
    size: usize,
    read: bool,
    write: bool,
    execute: bool,
) -> bool {
    let raw = PROTECT_FN.load(Ordering::Acquire);
    if raw == 0 {
        return false;
    }
    // 安全性：注册入口只接受函数指针，写入和读取使用 Release/Acquire 配对。
    let protect: ProtectElmImageRangeFn = unsafe { core::mem::transmute(raw) };
    protect(vaddr, size, read, write, execute)
}

pub fn sync_elm_image_icache() -> bool {
    let raw = SYNC_ICACHE_FN.load(Ordering::Acquire);
    if raw == 0 {
        return false;
    }
    // 安全性：注册入口只接受函数指针，写入和读取使用 Release/Acquire 配对。
    let sync_icache: SyncElmImageIcacheFn = unsafe { core::mem::transmute(raw) };
    sync_icache();
    true
}
