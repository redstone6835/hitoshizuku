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
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let state = arch::LoongArch64InterruptOps::save_interrupt_state();
        arch::LoongArch64InterruptOps::disable_interrupts();
        state
    }

    #[cfg(target_arch = "riscv64")]
    unsafe {
        arch::riscv64::trap::Riscv64InterruptOps::save_and_disable()
    }
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
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        arch::LoongArch64InterruptOps::restore_interrupt_state(state);
    }

    #[cfg(target_arch = "riscv64")]
    unsafe {
        arch::riscv64::trap::Riscv64InterruptOps::restore_interrupt_state(state);
    }
}

/// 常驻架构层 IMSIC 配置的代次句柄。
#[cfg(target_arch = "riscv64")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RiscvImsicHandle(u64);

#[cfg(target_arch = "riscv64")]
impl RiscvImsicHandle {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// 安装 IMSIC identity 上限和可用逻辑 CPU 集合。
#[cfg(target_arch = "riscv64")]
#[kernel_symbols::export(
    name = "hal.interrupt.riscv_imsic_install",
    contract = "kernel.hal.riscv-imsic@1",
    version = 1,
    capabilities = kernel_symbols::capability::HAL_CONTROL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
        | kernel_symbols::KERNEL_SYMBOL_FLAG_RETURNS_OWNED
)]
pub fn riscv_imsic_install(num_ids: u32, cpu_mask: u64) -> Option<RiscvImsicHandle> {
    arch::riscv64::aia::install_imsic_config(num_ids, cpu_mask).map(RiscvImsicHandle)
}

/// 撤销一代 IMSIC CSR 配置。
#[cfg(target_arch = "riscv64")]
#[kernel_symbols::export(
    name = "hal.interrupt.riscv_imsic_uninstall",
    contract = "kernel.hal.riscv-imsic@1",
    version = 1,
    capabilities = kernel_symbols::capability::HAL_CONTROL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn riscv_imsic_uninstall(handle: RiscvImsicHandle) -> bool {
    arch::riscv64::aia::uninstall_imsic_config(handle.0)
}

/// 更新目标 interrupt file 的 identity enable 位。
#[cfg(target_arch = "riscv64")]
#[kernel_symbols::export(
    name = "hal.interrupt.riscv_imsic_set_identity_enabled",
    contract = "kernel.hal.riscv-imsic@1",
    version = 1,
    capabilities = kernel_symbols::capability::HAL_CONTROL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn riscv_imsic_set_identity_enabled(
    handle: RiscvImsicHandle,
    cpu: usize,
    id: u32,
    enabled: bool,
) -> bool {
    arch::riscv64::aia::set_imsic_identity_enabled(handle.0, cpu, id, enabled)
}

/// 清除目标 interrupt file 上可能残留的 pending identity。
#[cfg(target_arch = "riscv64")]
#[kernel_symbols::export(
    name = "hal.interrupt.riscv_imsic_clear_identity",
    contract = "kernel.hal.riscv-imsic@1",
    version = 1,
    capabilities = kernel_symbols::capability::HAL_CONTROL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn riscv_imsic_clear_identity(handle: RiscvImsicHandle, cpu: usize, id: u32) -> bool {
    arch::riscv64::aia::clear_imsic_identity(handle.0, cpu, id)
}

/// 立即同步当前 hart 的 IMSIC 间接 CSR。
#[cfg(target_arch = "riscv64")]
#[kernel_symbols::export(
    name = "hal.interrupt.riscv_imsic_sync_current",
    contract = "kernel.hal.riscv-imsic@1",
    version = 1,
    capabilities = kernel_symbols::capability::HAL_CONTROL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn riscv_imsic_sync_current() {
    arch::riscv64::aia::sync_current_cpu();
}

/// claim/complete 当前 hart 的最高优先级 IMSIC identity。
#[cfg(target_arch = "riscv64")]
#[kernel_symbols::export(
    name = "hal.interrupt.riscv_imsic_claim",
    contract = "kernel.hal.riscv-imsic@1",
    version = 1,
    capabilities = kernel_symbols::capability::HAL_CONTROL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn riscv_imsic_claim() -> Option<u32> {
    arch::riscv64::aia::claim_imsic_identity()
}
