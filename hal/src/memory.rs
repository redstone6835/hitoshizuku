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

/// 排序普通内存、DMA 与设备 MMIO 访问。
///
/// 这是 ELM 驱动使用的架构无关强屏障。它主要覆盖“descriptor 写入 → doorbell”
/// 与“完成标志读取 → DMA payload 读取”两类设备协议边界。
#[kernel_symbols::export(
    name = "hal.memory.device_io_barrier",
    contract = "kernel.hal.memory@2",
    version = 2,
    capabilities = kernel_symbols::capability::HAL_QUERY
)]
pub fn device_io_barrier() {
    arch::device_io_barrier();
}
