//! x86_64 架构的纯 ABI 定义。
//!
//! 完整硬件后端接入前，本模块只公开不依赖汇编或 TrapFrame 的 syscall ABI。

pub mod syscall;

/// x86_64 动态链接器不需要架构兼容补丁。
pub fn patch_interpreter_image(_interp: &str, _bytes: &mut [u8]) {}

/// x86 固件应通过 ACPI `_CRS` 描述 PCI 窗口，不提供猜测范围。
pub const fn default_pci_mmio_window() -> Option<core::ops::Range<u64>> {
    None
}

/// x86 CPU 缓存与常规 PCI DMA 保持硬件一致性。
pub const fn acpi_pci_dma_coherent_default() -> bool {
    true
}

/// IOMMU 启用前，x86 PCI 主机使用 CPU 物理地址作为 DMA 地址。
pub const fn acpi_pci_identity_dma_default() -> bool {
    true
}

/// CPUID/HWCAP 发布将在完整 x86 后端中接入；当前不声明可选能力。
pub const fn user_hwcap() -> usize {
    0
}
