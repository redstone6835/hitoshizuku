//! RISC-V64 Sv48 分页实现。
//!
//! `general::PagingArch` 的 RISC-V64 后端。Sv48 四级页表，支持 4KiB / 2MiB / 1GiB 页。

use crate::riscv64::specific::*;
use general::{PagingArch, PhysPageTableRoot, VirtAddr};

// ── PTE 位域 ──────────────────────────────────────────────────────────────────

const PTE_V: usize = 1 << 0;
const PTE_R: usize = 1 << 1;
const PTE_W: usize = 1 << 2;
const PTE_X: usize = 1 << 3;
const PTE_U: usize = 1 << 4;
const PTE_G: usize = 1 << 5;
const PTE_A: usize = 1 << 6;
const PTE_D: usize = 1 << 7;

// ── 页表参数 ──────────────────────────────────────────────────────────────────

/// 页内偏移位数（4KiB = 2^12）。
pub const PAGE_SHIFT: usize = 12;
/// Sv48 页表层数。
pub const SV48_LEVELS: usize = 4;
const VPN_MASK: usize = 0x1FF;
const PPN_BITS: usize = 44;
const PPN_FIELD_MASK: usize = (1 << PPN_BITS) - 1;
const ASID_BITS: usize = 16;
const ASID_MASK: usize = (1 << ASID_BITS) - 1;

/// 支持的叶 PTE 层级（level 1 = 1GiB, level 2 = 2MiB, level 3 = 4KiB）。
pub const SUPPORTED_LEAF_LEVELS: [usize; 3] = [1, 2, 3];

// ── 类型 ──────────────────────────────────────────────────────────────────────

/// Sv48 页表项原始位表示。
///
/// 透明包装 `usize`，保留底层位级布局，在 trait 边界上提供类型安全。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Riscv64Pte(pub usize);

/// Sv48 PTE 权限位字段（低 10 位）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Riscv64Flags(pub usize);

impl Riscv64Flags {
    #[inline]
    pub const fn bits(self) -> usize {
        self.0
    }
}

impl Riscv64Pte {
    #[inline]
    pub const fn bits(self) -> usize {
        self.0
    }
}

// ── Riscv64Paging ─────────────────────────────────────────────────────────────

/// RISC-V64 Sv48 分页实现体。
pub struct Riscv64Paging;

impl Riscv64Paging {
    const fn level_page_shift(level: usize) -> usize {
        PAGE_SHIFT + 9 * (SV48_LEVELS - 1 - level)
    }

    #[inline]
    fn level_page_size(level: usize) -> Option<usize> {
        if !SUPPORTED_LEAF_LEVELS.contains(&level) || level >= SV48_LEVELS {
            return None;
        }
        Some(1usize << Self::level_page_shift(level))
    }

    const fn make_ppn(paddr: usize) -> usize {
        (paddr >> PAGE_SHIFT) & PPN_FIELD_MASK
    }

    const fn leaf_flags(r: bool, w: bool, x: bool, u: bool, g: bool) -> usize {
        let mut f = PTE_V | PTE_A;
        if w {
            f |= PTE_D;
        }
        if r {
            f |= PTE_R;
        }
        if w {
            f |= PTE_W;
        }
        if x {
            f |= PTE_X;
        }
        if u {
            f |= PTE_U;
        }
        if g {
            f |= PTE_G;
        }
        f
    }

    #[inline]
    pub const fn invalid_pte() -> Riscv64Pte {
        Riscv64Pte(0)
    }

    #[inline]
    pub const fn make_table_pte(next_table_paddr: usize) -> Riscv64Pte {
        Riscv64Pte((Self::make_ppn(next_table_paddr) << 10) | PTE_V)
    }

    #[inline]
    pub const fn make_leaf_pte(
        pa: usize,
        r: bool,
        w: bool,
        x: bool,
        u: bool,
        g: bool,
    ) -> Riscv64Pte {
        Riscv64Pte((Self::make_ppn(pa) << 10) | Self::leaf_flags(r, w, x, u, g))
    }

    /// 激活页表根并设置 ASID。
    ///
    /// # Safety
    ///
    /// 调用者必须保证：
    /// - `root` 指向有效且完整的 Sv48 根页表物理页
    /// - `asid` 与目标地址空间匹配
    /// - 调用发生在上下文切换临界区（中断已关闭或不会被抢占）
    pub unsafe fn activate_with_asid(root: PhysPageTableRoot, asid: usize) {
        let satp =
            SATP_MODE_SV48 | ((asid & ASID_MASK) << PPN_BITS) | (Self::make_ppn(root.as_usize()));
        unsafe {
            core::arch::asm!(
                "csrw satp, {val}",
                "sfence.vma",
                val = in(reg) satp,
                options(nostack, preserves_flags)
            );
        }
    }

    /// 读取当前 hart 的 ASID（从 satp CSR 中提取）。
    pub fn current_asid() -> usize {
        let satp: usize;
        unsafe {
            core::arch::asm!("csrr {v}, satp", v = out(reg) satp, options(nostack, preserves_flags));
        }
        (satp >> PPN_BITS) & ASID_MASK
    }

    /// 按 (vaddr, asid) 精确刷新 TLB。
    ///
    /// 硬件语义：`sfence.vma rs1, rs2`
    /// - rs1 = 虚拟地址（x0 寄存器表示匹配所有地址）
    /// - rs2 = ASID（x0 寄存器表示匹配所有 ASID）
    ///
    /// 注意：当 `asid` 参数值为 0 时，编译器会将 0 加载到一个通用寄存器中，
    /// 硬件将其视为"flush ASID 0"而非"flush 所有 ASID"。若需全局 flush，
    /// 应传 `vaddr = None`（走无操作数的 `sfence.vma` 路径）。
    ///
    /// # Safety
    ///
    /// 必须在 S-mode 下调用。
    #[inline]
    pub unsafe fn flush_tlb_with_asid(asid: usize, vaddr: Option<VirtAddr>) {
        unsafe {
            if let Some(addr) = vaddr {
                core::arch::asm!(
                    "sfence.vma {va}, {asid}",
                    va = in(reg) addr.as_usize(),
                    asid = in(reg) asid,
                    options(nostack)
                );
            } else {
                core::arch::asm!("sfence.vma", options(nostack));
            }
        }
    }

    /// 使用当前 ASID 刷新 TLB。
    ///
    /// # Safety
    ///
    /// 同 [`flush_tlb_with_asid`](Self::flush_tlb_with_asid)。
    #[inline]
    pub unsafe fn flush_tlb_current_asid(vaddr: Option<VirtAddr>) {
        unsafe {
            Self::flush_tlb_with_asid(Self::current_asid(), vaddr);
        }
    }
}

// ── PagingArch trait 实现 ─────────────────────────────────────────────────────

impl PagingArch for Riscv64Paging {
    type Pte = Riscv64Pte;
    type Flags = Riscv64Flags;

    const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
    const LEVELS: usize = SV48_LEVELS;
    const ENTRIES_PER_TABLE: usize = 512;

    /// Sv48 规范地址检查。
    ///
    /// bit[47] 必须符号扩展到 bit[63:48]：
    /// - 低半：bit[47]=0 → bit[63:48] 全 0
    /// - 高半：bit[47]=1 → bit[63:48] 全 1
    fn is_canonical_vaddr(vaddr: usize) -> bool {
        let sign = (vaddr >> 47) & 1;
        let upper = vaddr >> 48;
        (upper == 0 && sign == 0) || (upper == 0xFFFF && sign == 1)
    }

    #[inline]
    fn level_index(vaddr: usize, level: usize) -> usize {
        let va48 = vaddr & ((1usize << 48) - 1);
        let shift = PAGE_SHIFT + 9 * (Self::LEVELS - 1 - level);
        (va48 >> shift) & VPN_MASK
    }

    #[inline]
    fn invalid_pte() -> Self::Pte {
        Riscv64Paging::invalid_pte()
    }
    #[inline]
    fn pte_is_valid(pte: Self::Pte) -> bool {
        pte.0 & PTE_V != 0
    }
    #[inline]
    fn pte_is_leaf(pte: Self::Pte) -> bool {
        pte.0 & PTE_V != 0 && pte.0 & (PTE_R | PTE_W | PTE_X) != 0
    }
    #[inline]
    fn pte_addr(pte: Self::Pte) -> usize {
        ((pte.0 >> 10) & PPN_FIELD_MASK) << PAGE_SHIFT
    }
    #[inline]
    fn pte_flags(pte: Self::Pte) -> Self::Flags {
        Riscv64Flags(pte.0 & 0x3FF)
    }

    #[inline]
    fn flags_readable(f: Self::Flags) -> bool {
        f.0 & PTE_R != 0
    }
    #[inline]
    fn flags_writable(f: Self::Flags) -> bool {
        f.0 & PTE_W != 0
    }
    #[inline]
    fn flags_executable(f: Self::Flags) -> bool {
        f.0 & PTE_X != 0
    }
    #[inline]
    fn flags_user_accessible(f: Self::Flags) -> bool {
        f.0 & PTE_U != 0
    }
    #[inline]
    fn flags_global(f: Self::Flags) -> bool {
        f.0 & PTE_G != 0
    }

    #[inline]
    fn make_table_pte(next_table: usize) -> Self::Pte {
        Riscv64Paging::make_table_pte(next_table)
    }

    #[inline]
    fn is_valid_leaf_perm(
        read: bool,
        write: bool,
        _execute: bool,
        _user: bool,
        _global: bool,
    ) -> bool {
        // RISC-V 规范：W=1 且 R=0 是保留编码，不允许使用。
        // 这是一个架构约束，必须在页表映射时检查。
        !(write && !read)
    }

    #[inline]
    fn make_leaf_pte(pa: usize, r: bool, w: bool, x: bool, u: bool, g: bool) -> Self::Pte {
        Riscv64Paging::make_leaf_pte(pa, r, w, x, u, g)
    }

    #[inline]
    fn supported_leaf_levels() -> &'static [usize] {
        &SUPPORTED_LEAF_LEVELS
    }
    #[inline]
    fn leaf_page_size(level: usize) -> Option<usize> {
        Riscv64Paging::level_page_size(level)
    }

    #[inline]
    fn make_leaf_pte_for_level(
        level: usize,
        pa: usize,
        r: bool,
        w: bool,
        x: bool,
        u: bool,
        g: bool,
    ) -> Option<Self::Pte> {
        let size = Riscv64Paging::level_page_size(level)?;
        if pa & (size - 1) != 0 {
            return None;
        }
        Some(Riscv64Paging::make_leaf_pte(pa, r, w, x, u, g))
    }

    unsafe fn activate(root: PhysPageTableRoot) {
        unsafe {
            Riscv64Paging::activate_with_asid(root, Riscv64Paging::current_asid());
        }
    }

    #[inline]
    fn pte_to_usize(pte: Self::Pte) -> usize {
        pte.0
    }
    #[inline]
    fn pte_from_usize(bits: usize) -> Self::Pte {
        Riscv64Pte(bits)
    }

    unsafe fn flush_tlb(vaddr: Option<VirtAddr>) {
        unsafe {
            Riscv64Paging::flush_tlb_current_asid(vaddr);
        }
    }
}
