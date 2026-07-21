//! 调度相关架构 hook 的 HAL 入口。

/// 注册调度、时间、trap、MM 与 syscall 所需的架构侧 ops。
pub fn register_arch_hooks() {
    #[cfg(any(target_arch = "loongarch64", target_arch = "riscv64"))]
    {
        arch::register_sched_ctx();
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::register_sched_ctx();
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SecondaryCpuReport {
    pub detected: usize,
    pub started: usize,
    pub failed: usize,
}

pub fn start_secondary_cpus() -> SecondaryCpuReport {
    #[cfg(any(target_arch = "loongarch64", target_arch = "riscv64"))]
    {
        let report = arch::start_secondary_cpus();
        return SecondaryCpuReport {
            detected: report.detected,
            started: report.started,
            failed: report.failed,
        };
    }

    #[cfg(not(any(target_arch = "loongarch64", target_arch = "riscv64")))]
    SecondaryCpuReport {
        detected: 1,
        started: 1,
        failed: 0,
    }
}
