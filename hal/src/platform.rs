//! 平台/架构元信息。

use core::ops::Range;

/// Linux `utsname.machine` 风格的架构名称。
pub fn arch_name() -> &'static str {
    #[cfg(target_arch = "loongarch64")]
    {
        "loongarch64"
    }

    #[cfg(target_arch = "riscv64")]
    {
        todo!("riscv64 HAL platform metadata is not implemented")
    }
}

/// 当前平台期望的用户态 ELF 架构。
pub fn elf_arch() -> elf::Arch {
    #[cfg(target_arch = "loongarch64")]
    {
        elf::Arch::LoongArch64
    }

    #[cfg(target_arch = "riscv64")]
    {
        todo!("riscv64 HAL ELF metadata is not implemented")
    }
}

/// 固件未分配 PCI BAR 时使用的默认 MMIO 窗口。
pub fn default_pci_mmio_window() -> Option<Range<u64>> {
    #[cfg(target_arch = "loongarch64")]
    {
        Some(0x4000_0000..0x8000_0000)
    }

    #[cfg(target_arch = "riscv64")]
    {
        todo!("riscv64 HAL PCI MMIO window is not implemented")
    }
}
