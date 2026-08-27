//! 平台/架构元信息。

use core::ops::Range;

use general::ArchitectureId;

/// 把 Cargo 编译目标归一化为内核通用架构身份。
///
/// 条件编译只存在于 HAL 胶水层；调用方不得再次按 `target_arch` 分叉。
pub const fn architecture_id() -> ArchitectureId {
    #[cfg(target_arch = "loongarch64")]
    {
        return ArchitectureId::LoongArch64;
    }

    #[cfg(target_arch = "riscv64")]
    {
        return ArchitectureId::Riscv64;
    }

    #[cfg(target_arch = "x86_64")]
    {
        return ArchitectureId::X86_64;
    }

    #[allow(unreachable_code)]
    ArchitectureId::Unknown
}

/// Linux `utsname.machine` 风格的架构名称。
#[kernel_symbols::export(name = "hal.platform.arch_name", contract = "kernel.hal.platform@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn arch_name() -> &'static str {
    architecture_id().name()
}

/// 当前平台期望的用户态 ELF 架构。
#[kernel_symbols::export(name = "hal.platform.elf_arch", contract = "kernel.hal.platform@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn elf_arch() -> elf::Arch {
    match architecture_id() {
        ArchitectureId::LoongArch64 => elf::Arch::LoongArch64,
        ArchitectureId::Riscv64 => elf::Arch::Riscv64,
        ArchitectureId::X86_64 => elf::Arch::X86_64,
        ArchitectureId::Unknown => elf::Arch::Unknown(0),
    }
}

/// 当前内核对应的 Hitoshizuku Native ABI wire 架构。
pub fn native_abi_arch() -> native_abi::TargetArch {
    match architecture_id() {
        ArchitectureId::LoongArch64 => native_abi::TargetArch::LoongArch64,
        ArchitectureId::Riscv64 => native_abi::TargetArch::Riscv64,
        ArchitectureId::X86_64 => native_abi::TargetArch::X86_64,
        ArchitectureId::Unknown => panic!("[hal] unsupported Native ABI architecture"),
    }
}

/// 当前内核对应的 ELM EBI wire 架构。
pub fn elm_ebi_arch() -> elm_model::ElmEbiArch {
    match architecture_id() {
        ArchitectureId::LoongArch64 => elm_model::ElmEbiArch::LoongArch64,
        ArchitectureId::Riscv64 => elm_model::ElmEbiArch::Riscv64,
        ArchitectureId::X86_64 => elm_model::ElmEbiArch::X86_64,
        ArchitectureId::Unknown => panic!("[hal] unsupported ELM EBI architecture"),
    }
}

/// Linux ELF auxv 中由架构后端发布的硬件能力位。
pub fn user_hwcap() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        return arch::loongarch64::user_hwcap();
    }

    #[cfg(target_arch = "riscv64")]
    {
        return arch::riscv64::user_hwcap();
    }

    #[cfg(target_arch = "x86_64")]
    {
        return arch::x86_64::user_hwcap();
    }

    #[allow(unreachable_code)]
    0
}

/// 固件未分配 PCI BAR 时使用的默认 MMIO 窗口。
#[kernel_symbols::export(name = "hal.platform.default_pci_mmio_window", contract = "kernel.hal.platform@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn default_pci_mmio_window() -> Option<Range<u64>> {
    #[cfg(target_arch = "loongarch64")]
    {
        return arch::loongarch64::default_pci_mmio_window();
    }

    #[cfg(target_arch = "riscv64")]
    {
        return arch::riscv64::default_pci_mmio_window();
    }

    #[cfg(target_arch = "x86_64")]
    {
        return arch::x86_64::default_pci_mmio_window();
    }

    #[allow(unreachable_code)]
    None
}

/// ACPI PCI root bridge 的架构默认 DMA 一致性策略。
pub fn acpi_pci_dma_coherent_default() -> bool {
    #[cfg(target_arch = "loongarch64")]
    {
        return arch::loongarch64::acpi_pci_dma_coherent_default();
    }

    #[cfg(target_arch = "riscv64")]
    {
        return arch::riscv64::acpi_pci_dma_coherent_default();
    }

    #[cfg(target_arch = "x86_64")]
    {
        return arch::x86_64::acpi_pci_dma_coherent_default();
    }

    #[allow(unreachable_code)]
    false
}

/// ACPI PCI root bridge 的架构默认 DMA 地址转换策略。
pub fn acpi_pci_identity_dma_default() -> bool {
    #[cfg(target_arch = "loongarch64")]
    {
        return arch::loongarch64::acpi_pci_identity_dma_default();
    }

    #[cfg(target_arch = "riscv64")]
    {
        return arch::riscv64::acpi_pci_identity_dma_default();
    }

    #[cfg(target_arch = "x86_64")]
    {
        return arch::x86_64::acpi_pci_identity_dma_default();
    }

    #[allow(unreachable_code)]
    false
}
