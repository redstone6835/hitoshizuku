//! 用户内存访问的安全包装。
//!
//! 上层 syscall 实现 / loader 调本模块的安全 API；本模块再通过注入的
//! [`super::ops::UserAccessOps`] 真正访问用户地址空间。所有底层 unsafe 集中
//! 在 arch 一侧，调用方拿到 `Result<_, UserAccessError>` 就足以决定 errno。
//!
//! ## 安全边界
//!
//! `dst`/`src` 是内核态裸内存（`&mut [u8]` / `&[u8]`），由 Rust 借用规则保
//! 证有效；user 地址纯数值，由 arch 的 `__ex_table` fixup 路径捕获缺页。
//! 任何"用户提供的指针越界 / 未映射"都会回归到 `Err(Fault)`，不会引发 panic。

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use mm::UserAccessError;

use crate::mm::ops::user_access_ops;
use crate::mm::vm_space::{UserReadWindows, UserWriteWindows, VmSpace, page_size};

const USER_COPY_WINDOWS: usize = 16;

/// 从用户地址 `user` 读 `dst.len()` 字节到 `dst`。
#[kernel_symbols::export(name = "general.mm.user_access.copy_from_user", contract = "kernel.mm.user-access@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn copy_from_user(user: usize, dst: &mut [u8]) -> Result<(), UserAccessError> {
    if dst.is_empty() {
        return Ok(());
    }
    if user.checked_add(dst.len()).is_none() {
        return Err(UserAccessError::Fault);
    }
    if let Some(vm) = current_task_vm_space() {
        let total = dst.len();
        let mut copied = 0usize;
        let mut windows = UserReadWindows::<USER_COPY_WINDOWS>::empty();
        while copied < total {
            let user_ptr = user.checked_add(copied).ok_or(UserAccessError::Fault)?;
            let chunk = user_copy_chunk_len(user_ptr, total - copied);
            vm.pin_user_read_windows_into(user_ptr, chunk, &mut windows)
                .map_err(|_| UserAccessError::Fault)?;
            windows
                .copy_into(0, &mut dst[copied..copied + chunk])
                .map_err(|_| UserAccessError::Fault)?;
            copied += chunk;
        }
        return Ok(());
    }

    let Some(ops) = user_access_ops() else {
        return Err(UserAccessError::Fault);
    };
    // Safety: dst 切片来自 Rust 借用，长度有效；user 由 arch 内部按 __ex_table
    //         fixup 处理任何故障。
    unsafe { (ops.copy_from_user)(dst.as_mut_ptr(), user, dst.len()) }
}

/// 把 `src` 写到用户地址 `user`。
#[kernel_symbols::export(name = "general.mm.user_access.copy_to_user", contract = "kernel.mm.user-access@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn copy_to_user(user: usize, src: &[u8]) -> Result<(), UserAccessError> {
    if src.is_empty() {
        return Ok(());
    }
    if user.checked_add(src.len()).is_none() {
        return Err(UserAccessError::Fault);
    }
    if let Some(vm) = current_task_vm_space() {
        let total = src.len();
        let mut copied = 0usize;
        let mut windows = UserWriteWindows::<USER_COPY_WINDOWS>::empty();
        while copied < total {
            let user_ptr = user.checked_add(copied).ok_or(UserAccessError::Fault)?;
            let chunk = user_copy_chunk_len(user_ptr, total - copied);
            vm.pin_user_write_windows_into(user_ptr, chunk, &mut windows)
                .map_err(|_| UserAccessError::Fault)?;
            windows
                .copy_from(0, &src[copied..copied + chunk])
                .map_err(|_| UserAccessError::Fault)?;
            copied += chunk;
        }
        return Ok(());
    }

    let Some(ops) = user_access_ops() else {
        return Err(UserAccessError::Fault);
    };
    // Safety: 同上。
    unsafe { (ops.copy_to_user)(user, src.as_ptr(), src.len()) }
}

/// 从用户地址读一段 NUL 结尾的 C 字符串，最多 `max` 字节（不含 NUL）。
#[kernel_symbols::export(name = "general.mm.user_access.copy_cstr_from_user", contract = "kernel.mm.user-access@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED)]
pub fn copy_cstr_from_user(user: usize, max: usize) -> Result<String, UserAccessError> {
    let Some(ops) = user_access_ops() else {
        return Err(UserAccessError::Fault);
    };
    // 第一步 strnlen，确认实际长度 / 是否超限。
    // Safety: arch 端实现 fixup。
    let len = unsafe { (ops.strnlen_user)(user, max)? };
    let mut buf: Vec<u8> = alloc::vec![0u8; len];
    // Safety: 同 copy_from_user。
    unsafe { (ops.copy_from_user)(buf.as_mut_ptr(), user, len)? };
    // 用户态可能在 strnlen 之后写入非 UTF-8 字节；这里转换失败回 Fault 而非
    // 自定义错误码——上层只关心"读到了"vs"读不到"。
    String::from_utf8(buf).map_err(|_| UserAccessError::Fault)
}

fn current_task_vm_space() -> Option<Arc<VmSpace>> {
    if !sched::is_ready() {
        return None;
    }
    // 只在复制 VmSpace Arc 前借用 current raw 槽，避免用户复制热路径为 Task
    // 额外获取 owning current 锁和增减一次强引用。
    let task = sched::current_task_ref();
    let payload = task.ext_lookup(sched::TASKEXT_VM_SPACE)?;
    payload.downcast::<VmSpace>().ok()
}

#[inline]
fn user_copy_chunk_len(user: usize, remaining: usize) -> usize {
    let page_size = page_size();
    let first_page = page_size - (user & (page_size - 1));
    remaining.min(first_page + (USER_COPY_WINDOWS - 1) * page_size)
}
