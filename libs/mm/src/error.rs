//! 用户内存访问的专用错误类型。
//!
//! 与 [`errno::Errno`] 分离：`Errno` 是面向 Linux ABI 的整体错误码空间，
//! [`UserAccessError`] 专门描述"尝试访问用户态缓冲"这一子类。上层 syscall
//! 实现拿到本错误后按需翻成 `EFAULT`/`EINVAL`。

use errno::Errno;

/// 用户内存访问失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAccessError {
    /// 触发缺页 / 权限错误，无法恢复到常规路径；arch 侧走 `__ex_table` fixup
    /// 之后最终归到这里。等价于 Linux `-EFAULT`。
    Fault,
    /// 起止地址未按所需粒度对齐。等价于 Linux `-EINVAL`。
    Misaligned,
    /// 长度超过调用方允许的上限（例如 `copy_cstr_from_user` 的 `max`）。
    /// 等价于 Linux `-EINVAL` 或 `-ENAMETOOLONG`（调用方决定）。
    TooLong,
    /// 内核无法为复制结果分配缓冲区。等价于 `-ENOMEM`。
    OutOfMemory,
}

impl UserAccessError {
    /// 翻译成 POSIX errno。上层 syscall 直接 `as_errno().as_i32()` 取负返回。
    pub const fn as_errno(self) -> Errno {
        match self {
            UserAccessError::Fault => Errno::EFAULT,
            UserAccessError::Misaligned => Errno::EINVAL,
            UserAccessError::TooLong => Errno::EINVAL,
            UserAccessError::OutOfMemory => Errno::ENOMEM,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::UserAccessError;

    #[test]
    fn allocation_failure_maps_to_enomem() {
        assert_eq!(
            UserAccessError::OutOfMemory.as_errno(),
            errno::Errno::ENOMEM
        );
    }
}
