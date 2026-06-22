//! 时间源封装。

/// 返回内核可用的单调纳秒时间戳。
pub fn monotonic_ns() -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::kernel_timestamp_ns()
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::kernel_timestamp_ns()
    }
}

/// 返回当前稳定计数器的原始周期值。
pub fn stable_counter_raw() -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::stable_counter_raw()
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::stable_counter_raw()
    }
}

/// 返回稳定计数器频率（Hz）。
pub fn stable_counter_hz() -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::stable_counter_hz()
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::stable_counter_hz()
    }
}

/// 将稳定计数器周期值换算为纳秒。
pub fn stable_counter_to_ns(cnt: u64) -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::stable_counter_to_ns(cnt)
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::stable_counter_to_ns(cnt)
    }
}
