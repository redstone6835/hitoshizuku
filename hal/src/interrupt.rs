//! 本地可屏蔽中断控制。
//!
//! 该模块只暴露构造短临界区所需的保存、关闭和恢复操作。调用方必须在同一 CPU
//! 上成对使用两个函数，并且不得把保存状态跨任务或跨 CPU 传递。

/// 保存当前 CPU 的本地中断状态并关闭可屏蔽中断。
///
/// 返回值是不透明的架构状态，只能传给 [`restore_local`]。嵌套调用是安全的：
/// 内层恢复不会打开外层已经关闭的中断。
#[kernel_symbols::export(
    name = "hal.interrupt.save_and_disable_local",
    contract = "kernel.hal.interrupt@1",
    version = 1,
    capabilities = kernel_symbols::capability::HAL_CONTROL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn save_and_disable_local() -> usize {
    arch::save_and_disable_local_interrupts()
}

/// 恢复当前 CPU 此前保存的本地中断状态。
///
/// `state` 必须由同一 CPU 上最近一层尚未恢复的 [`save_and_disable_local`] 返回。
#[kernel_symbols::export(
    name = "hal.interrupt.restore_local",
    contract = "kernel.hal.interrupt@1",
    version = 1,
    capabilities = kernel_symbols::capability::HAL_CONTROL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn restore_local(state: usize) {
    arch::restore_local_interrupts(state);
}
