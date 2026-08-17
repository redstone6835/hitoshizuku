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

/// 设备读取前对内核虚拟地址范围做非 coherent DMA 的 clean（CPU→设备）。
///
/// 架构不支持（如无 Zicbom）时为空转；调用方需保证范围有效映射。
#[kernel_symbols::export(
    name = "hal.memory.dma_clean_range",
    contract = "kernel.hal.memory@2",
    version = 2,
    capabilities = kernel_symbols::capability::HAL_QUERY
)]
pub fn dma_clean_range(vaddr: usize, len: usize) {
    #[cfg(target_arch = "riscv64")]
    {
        // Safety: 调用方保证范围是有效映射的内核内存。
        unsafe { arch::clean_dcache_range(vaddr, len) };
    }

    #[cfg(target_arch = "loongarch64")]
    {
        let _ = (vaddr, len);
    }
}

/// 设备写入后对内核虚拟地址范围做非 coherent DMA 的 invalidate（设备→CPU）。
///
/// 架构不支持（如无 Zicbom）时为空转；调用方需保证范围内无未写回设备的 CPU
/// 脏数据。
#[kernel_symbols::export(
    name = "hal.memory.dma_invalidate_range",
    contract = "kernel.hal.memory@2",
    version = 2,
    capabilities = kernel_symbols::capability::HAL_QUERY
)]
pub fn dma_invalidate_range(vaddr: usize, len: usize) {
    #[cfg(target_arch = "riscv64")]
    {
        // Safety: 调用方保证范围是有效映射的内核内存。
        unsafe { arch::invalidate_dcache_range(vaddr, len) };
    }

    #[cfg(target_arch = "loongarch64")]
    {
        let _ = (vaddr, len);
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
