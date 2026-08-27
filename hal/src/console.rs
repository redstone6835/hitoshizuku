//! 早期控制台输出。

/// 输出原始字节到架构早期控制台。
///
/// 在内核控制台子系统就绪前用于日志/测试输出。
#[kernel_symbols::export(name = "hal.console.early_write_bytes", contract = "kernel.hal.console@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn early_write_bytes(bytes: &[u8]) {
    arch::e_write_bytes(bytes);
}

/// 输出不依赖通用日志或控制台状态的板级启动标记。
pub fn raw_debug_byte(byte: u8) {
    arch::raw_debug_byte(byte);
}

/// 输出不依赖通用日志或控制台状态的板级十六进制启动标记。
pub fn raw_debug_hex16(value: usize) {
    arch::raw_debug_hex16(value);
}
