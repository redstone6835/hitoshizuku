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

/// 设备读取前清理覆盖内核虚拟地址范围的 cache block。
///
/// 返回 `false` 表示当前架构无法为非一致性 DMA 提供所需维护操作；驱动必须
/// 中止传输。LoongArch 当前只对 coherent 设备使用该 HAL，因此无需执行 CMO。
#[kernel_symbols::export(
    name = "hal.memory.dma_clean_range",
    contract = "kernel.hal.memory@2",
    version = 2,
    capabilities = kernel_symbols::capability::HAL_QUERY
)]
pub fn dma_clean_range(vaddr: usize, len: usize) -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        // Safety: HAL 契约要求调用者提供有效、由其拥有的内核映射范围。
        unsafe { arch::clean_dcache_range(vaddr, len) }
    }

    #[cfg(target_arch = "loongarch64")]
    {
        let _ = (vaddr, len);
        true
    }
}

/// 设备写入前后失效覆盖内核虚拟地址范围的 cache block。
#[kernel_symbols::export(
    name = "hal.memory.dma_invalidate_range",
    contract = "kernel.hal.memory@2",
    version = 2,
    capabilities = kernel_symbols::capability::HAL_QUERY
)]
pub fn dma_invalidate_range(vaddr: usize, len: usize) -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        // Safety: HAL 契约要求调用者保证范围有效且没有待保留的 CPU 脏数据。
        unsafe { arch::invalidate_dcache_range(vaddr, len) }
    }

    #[cfg(target_arch = "loongarch64")]
    {
        let _ = (vaddr, len);
        true
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
