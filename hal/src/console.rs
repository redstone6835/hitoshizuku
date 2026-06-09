//! 早期控制台输出。

/// 输出原始字节到架构早期控制台。
///
/// 在内核控制台子系统就绪前用于日志/测试输出。
pub fn early_write_bytes(bytes: &[u8]) {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::e_write_bytes(bytes);
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::early_console::e_write_bytes(bytes);
    }
}
