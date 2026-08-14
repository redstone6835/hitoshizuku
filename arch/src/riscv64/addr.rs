//! RISC-V64 虚拟/物理地址转换与空间常量。
//!
//! Sv39/Sv48 共用线性偏移直映：`VA = PA + KERNEL_VA_OFFSET`。与 LoongArch 的 DMW
//! 硬件窗口不同，RISC-V 的直映是通过页表实现的（boot 阶段建立 1GiB leaf 映射），
//! 但对软件而言转换公式完全相同——加减固定偏移即可。
//!
//! 地址空间布局（Sv39/Sv48 共同高半区）：
//! ```text
//! 0xFFFF_FFC0_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF  — 共同规范窗口（256 GiB）
//! ```

/// 内核直接映射偏移：`VA = PA + KERNEL_VA_OFFSET`。
///
/// Sv39 高半区起始地址；该地址同时满足 Sv48 的规范地址要求。
pub const KERNEL_VA_OFFSET: usize = 0xFFFF_FFC0_0000_0000;

/// 物理地址 → 内核虚拟地址（线性偏移直映）。
///
/// # 参数
/// - `paddr`: 物理地址。
///
/// # 返回值
/// 对应的内核态虚拟地址。
#[inline]
pub fn phys_to_virt(paddr: usize) -> usize {
    paddr.wrapping_add(KERNEL_VA_OFFSET)
}

/// 内核虚拟地址 → 物理地址（线性偏移逆运算）。
///
/// # 参数
/// - `vaddr`: 内核态虚拟地址（必须位于直映区）。
///
/// # 返回值
/// 对应的物理地址。
#[inline]
pub fn virt_to_phys(vaddr: usize) -> usize {
    vaddr.wrapping_sub(KERNEL_VA_OFFSET)
}

/// 将任意地址投影回直映区虚拟地址。
///
/// 先剥离高位窗口前缀得到物理地址，再重新加上直映偏移。
/// 用于 loader 在早期统一不同来源的指针。
#[inline]
pub fn reset_to_virt(ptr: usize) -> usize {
    phys_to_virt(virt_to_phys(ptr))
}
