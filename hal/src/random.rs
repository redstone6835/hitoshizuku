//! 随机子系统架构相关 hook 的 HAL 入口。
//!
//! `general::dev::random_source` 定义了 `EntropySource` trait，但
//! `general` 自身不能依赖 `arch`。kernel 启动时通过 `hal::random`
//! 提供的 `register_arch_hooks()` 间接调用 `arch::register_entropy_source`，
//! 把 loongarch64 / riscv64 的熵源挂到 `general` 的全局注册表里。

/// 把本架构的 `EntropySource` 装到 `general` 随机子系统里。
pub fn register_arch_hooks() {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::register_entropy_source();
    }

    #[cfg(target_arch = "riscv64")]
    {
        todo!("not implemented");
    }
}
