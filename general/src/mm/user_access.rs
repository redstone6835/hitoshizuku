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
#[kernel_symbols::export(name = "general.mm.user_access.copy_from_user", contract = "kernel.mm.user-access@1", version = 1, capabilities = kernel_symbols::capability::MM_MEMORY, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn copy_from_user(user: usize, dst: &mut [u8]) -> Result<(), UserAccessError> {
    if dst.is_empty() {
        return Ok(());
    }
    if user.checked_add(dst.len()).is_none() {
        return Err(UserAccessError::Fault);
    }
    let Some(ops) = user_access_ops() else {
        return Err(UserAccessError::Fault);
    };
    copy_from_user_with_ops(ops, user, dst)
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
    let Some(ops) = user_access_ops() else {
        return Err(UserAccessError::Fault);
    };
    copy_to_user_with_ops(ops, user, src)
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

#[inline]
fn copy_from_user_with_ops(
    ops: &crate::mm::UserAccessOps,
    user: usize,
    dst: &mut [u8],
) -> Result<(), UserAccessError> {
    // Safety: dst 切片来自 Rust 借用，长度有效；user 由 arch 的异常表路径捕获
    // lazy fault、权限错误和越界访问。
    unsafe { (ops.copy_from_user)(dst.as_mut_ptr(), user, dst.len()) }
}

#[inline]
fn copy_to_user_with_ops(
    ops: &crate::mm::UserAccessOps,
    user: usize,
    src: &[u8],
) -> Result<(), UserAccessError> {
    // Safety: src 切片有效；arch 在缺页时先调用当前 VmSpace 修复映射，无法修复
    // 才通过异常表返回 EFAULT。共享文件页首次写入仍由写缺页路径标脏。
    unsafe { (ops.copy_to_user)(user, src.as_ptr(), src.len()) }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use mm::UserAccessError;

    use super::{copy_from_user_with_ops, copy_to_user_with_ops};
    use crate::mm::UserAccessOps;

    static READ_CALLS: AtomicUsize = AtomicUsize::new(0);
    static WRITE_CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe fn test_copy_from_user(
        dst: *mut u8,
        src_user: usize,
        len: usize,
    ) -> Result<(), UserAccessError> {
        READ_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { core::ptr::copy_nonoverlapping(src_user as *const u8, dst, len) };
        Ok(())
    }

    unsafe fn test_copy_to_user(
        dst_user: usize,
        src: *const u8,
        len: usize,
    ) -> Result<(), UserAccessError> {
        WRITE_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { core::ptr::copy_nonoverlapping(src, dst_user as *mut u8, len) };
        Ok(())
    }

    unsafe fn test_strnlen_user(_: usize, _: usize) -> Result<usize, UserAccessError> {
        unreachable!("本测试不调用 strnlen_user")
    }

    static TEST_OPS: UserAccessOps = UserAccessOps {
        copy_from_user: test_copy_from_user,
        copy_to_user: test_copy_to_user,
        strnlen_user: test_strnlen_user,
    };

    #[test]
    fn ordinary_usercopy_uses_arch_operations_directly() {
        READ_CALLS.store(0, Ordering::Relaxed);
        WRITE_CALLS.store(0, Ordering::Relaxed);

        let source = [1u8, 2, 3, 4];
        let mut kernel = [0u8; 4];
        copy_from_user_with_ops(&TEST_OPS, source.as_ptr() as usize, &mut kernel)
            .expect("架构读路径应成功");
        assert_eq!(kernel, source);
        assert_eq!(READ_CALLS.load(Ordering::Relaxed), 1);

        let source = [5u8, 6, 7, 8];
        let mut user = [0u8; 4];
        copy_to_user_with_ops(&TEST_OPS, user.as_mut_ptr() as usize, &source)
            .expect("架构写路径应成功");
        assert_eq!(user, source);
        assert_eq!(WRITE_CALLS.load(Ordering::Relaxed), 1);
    }
}
