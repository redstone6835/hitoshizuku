//! 时间源封装。

/// 返回内核可用的单调纳秒时间戳。
#[kernel_symbols::export(name = "hal.time.monotonic_ns", contract = "kernel.hal.time@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn monotonic_ns() -> u64 {
    arch::kernel_timestamp_ns()
}

/// 返回当前稳定计数器的原始周期值。
#[kernel_symbols::export(name = "hal.time.stable_counter_raw", contract = "kernel.hal.time@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn stable_counter_raw() -> u64 {
    arch::stable_counter_raw()
}

/// 返回稳定计数器频率（Hz）。
#[kernel_symbols::export(name = "hal.time.stable_counter_hz", contract = "kernel.hal.time@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn stable_counter_hz() -> u64 {
    arch::stable_counter_hz()
}

/// 将稳定计数器周期值换算为纳秒。
#[kernel_symbols::export(name = "hal.time.stable_counter_to_ns", contract = "kernel.hal.time@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn stable_counter_to_ns(cnt: u64) -> u64 {
    arch::stable_counter_to_ns(cnt)
}
