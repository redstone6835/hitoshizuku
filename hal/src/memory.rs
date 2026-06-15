//! 内存布局相关的 HAL 查询接口。

/// 当前架构用户页粒度。
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
