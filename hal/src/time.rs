//! 时间源封装。

/// 返回内核可用的单调纳秒时间戳。
pub fn monotonic_ns() -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::kernel_timestamp_ns()
    }

    #[cfg(target_arch = "riscv64")]
    {
        todo!("riscv64 HAL timer is not implemented")
    }
}
