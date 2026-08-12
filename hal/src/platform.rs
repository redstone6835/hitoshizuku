//! 平台/架构元信息。

use core::ops::Range;

/// Linux `utsname.machine` 风格的架构名称。
#[kernel_symbols::export(name = "hal.platform.arch_name", contract = "kernel.hal.platform@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn arch_name() -> &'static str {
    #[cfg(target_arch = "loongarch64")]
    {
        "loongarch64"
    }

    #[cfg(target_arch = "riscv64")]
    {
        "riscv64"
    }
}

/// 当前平台期望的用户态 ELF 架构。
#[kernel_symbols::export(name = "hal.platform.elf_arch", contract = "kernel.hal.platform@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn elf_arch() -> elf::Arch {
    #[cfg(target_arch = "loongarch64")]
    {
        elf::Arch::LoongArch64
    }

    #[cfg(target_arch = "riscv64")]
    {
        elf::Arch::Riscv64
    }
}

/// 固件未分配 PCI BAR 时使用的默认 MMIO 窗口。
#[kernel_symbols::export(name = "hal.platform.default_pci_mmio_window", contract = "kernel.hal.platform@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn default_pci_mmio_window() -> Option<Range<u64>> {
    #[cfg(target_arch = "loongarch64")]
    {
        Some(0x4000_0000..0x8000_0000)
    }

    #[cfg(target_arch = "riscv64")]
    {
        Some(0x4000_0000..0x8000_0000)
    }
}
