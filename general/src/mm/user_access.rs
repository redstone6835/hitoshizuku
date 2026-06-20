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
use alloc::vec::Vec;

use mm::UserAccessError;

use crate::mm::ops::user_access_ops;

/// 从用户地址 `user` 读 `dst.len()` 字节到 `dst`。
#[inline]
pub fn copy_from_user(user: usize, dst: &mut [u8]) -> Result<(), UserAccessError> {
    let Some(ops) = user_access_ops() else {
        return Err(UserAccessError::Fault);
    };
    unsafe { (ops.copy_from_user)(dst.as_mut_ptr(), user, dst.len()) }
}

/// 把 `src` 写到用户地址 `user`。
#[inline]
pub fn copy_to_user(user: usize, src: &[u8]) -> Result<(), UserAccessError> {
    let Some(ops) = user_access_ops() else {
        return Err(UserAccessError::Fault);
    };
    unsafe { (ops.copy_to_user)(user, src.as_ptr(), src.len()) }
}

/// 从用户地址读一段 NUL 结尾的 C 字符串，最多 `max` 字节（不含 NUL）。
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
