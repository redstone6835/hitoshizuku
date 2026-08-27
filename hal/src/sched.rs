//! 调度相关架构 hook 的 HAL 入口。

/// 注册调度、时间、trap、MM 与 syscall 所需的架构侧 ops。
pub fn register_arch_hooks() {
    arch::register_sched_ctx();
}

/// 将当前 CPU 切回内核地址空间。
///
/// 用户任务切换到 idle 或内核线程时，调用方必须执行此操作，不能把旧用户
/// 地址空间留在硬件根寄存器中。各架构负责处理自己的根页表、ASID 与 TLB 语义。
pub fn activate_kernel_address_space() {
    arch::activate_kernel_page_table();
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SecondaryCpuReport {
    pub detected: usize,
    pub started: usize,
    pub failed: usize,
}

pub fn start_secondary_cpus() -> SecondaryCpuReport {
    let report = arch::start_secondary_cpus();
    SecondaryCpuReport {
        detected: report.detected,
        started: report.started,
        failed: report.failed,
    }
}
