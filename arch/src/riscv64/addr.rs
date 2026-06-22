//! RISC-V64 虚拟/物理地址转换与空间常量。
//!
//! Sv48 采用线性偏移直映：`VA = PA + KERNEL_VA_OFFSET`。与 LoongArch 的 DMW
//! 硬件窗口不同，RISC-V 的直映是通过页表实现的（boot 阶段建立 1GiB leaf 映射），
//! 但对软件而言转换公式完全相同——加减固定偏移即可。
//!
//! 地址空间布局（Sv48，高半区）：
//! ```text
//! 0xFFFF_FF80_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF  — 内核直映（512 GiB）
//!   PGD[511] → 内核 code + heap
//!   PGD[510] → MMIO 直映
//! ```

/// 内核直接映射偏移：`VA = PA + KERNEL_VA_OFFSET`。
///
/// Sv48 高半区起始地址（0xFFFF_FF80_0000_0000），对应 PGD[511] 覆盖的 512 GiB 范围。
/// 这是 RISC-V Sv48 页表模式的标准常量，由架构规范定义。
pub const KERNEL_VA_OFFSET: usize = 0xFFFF_FF80_0000_0000;

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
