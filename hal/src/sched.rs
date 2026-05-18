//! 调度相关架构 hook 的 HAL 入口。

/// 注册调度、时间、trap、MM 与 syscall 所需的架构侧 ops。
pub fn register_arch_hooks() {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::register_sched_ctx();
    }

    #[cfg(target_arch = "riscv64")]
    {
        todo!("riscv64 HAL sched hooks are not implemented")
    }
}
