//! 内存布局相关的 HAL 查询接口。

/// 当前架构用户页粒度。
#[kernel_symbols::export(name = "hal.memory.page_size", contract = "kernel.hal.memory@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn page_size() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        general::mm::page_size()
    }

    #[cfg(target_arch = "riscv64")]
    {
        general::mm::page_size()
    }
}

#[kernel_symbols::export(name = "hal.memory.virt_to_phys", contract = "kernel.hal.memory@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn virt_to_phys(virt: usize) -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        arch::virt_to_phys(virt)
    }

    #[cfg(target_arch = "riscv64")]
    {
        arch::virt_to_phys(virt)
    }
}
