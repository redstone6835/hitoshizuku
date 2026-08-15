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
        if r {
            f |= PTE_R;
        }
        if w {
            f |= PTE_W | PTE_D;
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

    #[inline]
    pub(crate) const fn satp_value(root: PhysPageTableRoot, asid: usize) -> usize {
        SATP_MODE_SV48 | ((asid & ASID_MASK) << PPN_BITS) | Self::make_ppn(root.as_usize())
    }

    #[inline]
    pub(crate) fn current_satp() -> usize {
        let current: usize;
        unsafe {
            core::arch::asm!(
                "csrr {current}, satp",
                current = out(reg) current,
                options(nostack, preserves_flags)
            );
        }
        current
    }

    /// 激活页表根并设置 ASID。
    ///
    /// # Safety
    ///
    /// 调用者必须保证：
    /// - `root` 指向有效且完整的 Sv48 根页表物理页
    /// - `asid` 与目标地址空间匹配
    /// - 调用发生在上下文切换临界区（中断已关闭或不会被抢占）
    pub unsafe fn activate_with_asid(
        root: PhysPageTableRoot,
        asid: usize,
        needs_page_table_fence: bool,
    ) {
        let current = Self::current_satp();
        unsafe {
            Self::activate_with_asid_from_current(root, asid, needs_page_table_fence, current)
        };
    }

    /// 使用调用方已经读取的 satp 完成激活，避免热路径重复读取 CSR。
    ///
    /// # Safety
    /// 与 [`activate_with_asid`] 相同；`current` 必须是进入本函数前同一 hart
    /// 读取的 satp，且调用期间不得被抢占或迁移。
    pub(crate) unsafe fn activate_with_asid_from_current(
        root: PhysPageTableRoot,
        asid: usize,
        needs_page_table_fence: bool,
        current: usize,
    ) {
        let satp = Self::satp_value(root, asid);
        if current == satp && !needs_page_table_fence {
            // 同一进程内的 pthread 切换会反复进入 VmSpace::activate。
            // root/asid 未变化时不需要重写 satp，也不能白白做全局 sfence.vma。
            return;
        }
        if current != satp {
            unsafe {
                core::arch::asm!(
                    "csrw satp, {val}",
                    val = in(reg) satp,
                    options(nostack, preserves_flags)
                );
            }
        }
        unsafe {
            // 非零 ASID 由 user_pgd 的 generation allocator 保证在当前代内唯一，
            // 旧地址空间的 translation 可以继续留在 TLB 中。硬件没有 ASID 时
            // 所有地址空间都退化为 ASID 0，切根后必须冲刷该 ASID。新建/修改后
            // 尚未 fence 的页表同样需要一次 ASID 定向 fence 来排序 PTE store。
            if asid == 0 || needs_page_table_fence {
                Self::flush_tlb_local_with_asid(asid, None);
            }
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
    /// 注意：即使 `asid` 的数值为 0，这里也会把它放进普通寄存器，因此硬件
    /// 仍把它解释为“只刷新 ASID 0”，而不是 rs2=`x0` 的“刷新所有 ASID”。
    /// 真正的全局刷新应调用 [`flush_tlb_global`](Self::flush_tlb_global)。
    ///
    /// # Safety
    ///
    /// 必须在 S-mode 下调用。
    #[inline]
    pub unsafe fn flush_tlb_with_asid(asid: usize, vaddr: Option<VirtAddr>) {
        unsafe { Self::flush_tlb_local_with_asid(asid, vaddr) };
        crate::riscv64::smp::remote_sfence_vma(Some(asid), vaddr.map(VirtAddr::as_usize));
    }

    /// 只刷新使用过目标用户地址空间的 CPU 集合。
    ///
    /// 本接口供用户 PGD 的活跃 CPU 跟踪使用。内核全局映射不得调用它，因为
    /// Global translation 仍要求覆盖全部在线 hart。
    #[inline]
    pub(crate) unsafe fn flush_tlb_with_asid_on_cpus(
        asid: usize,
        vaddr: Option<VirtAddr>,
        cpu_mask: usize,
    ) {
        let current = crate::riscv64::specific::current_cpu_id();
        if current < usize::BITS as usize && cpu_mask & (1usize << current) != 0 {
            unsafe { Self::flush_tlb_local_with_asid(asid, vaddr) };
        }
        crate::riscv64::smp::remote_sfence_vma_on(
            cpu_mask,
            Some(asid),
            vaddr.map(VirtAddr::as_usize),
        );
    }

    /// 排序先前页表写，并只失效当前 hart 的目标 ASID translation。
    ///
    /// 本接口不发 SBI RFENCE。调用者只有在确认不存在旧有效映射，或只是收敛
    /// 当前 hart 缓存的无效 translation 时才能单独使用它。
    ///
    /// # Safety
    ///
    /// 必须在 S-mode 下调用；解除映射、权限变化和物理页替换仍需使用同步多核
    /// 失效接口。
    #[inline]
    pub(crate) unsafe fn flush_tlb_local_with_asid(asid: usize, vaddr: Option<VirtAddr>) {
        unsafe {
            if let Some(addr) = vaddr {
                core::arch::asm!(
                    "sfence.vma {va}, {asid}",
                    va = in(reg) addr.as_usize(),
                    asid = in(reg) asid,
                    options(nostack)
                );
            } else {
                core::arch::asm!(
                    "sfence.vma zero, {asid}",
                    asid = in(reg) asid,
                    options(nostack)
                );
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

    /// 刷新所有 ASID（包括 Global translation）对应的 TLB 项。
    ///
    /// 内核高半区映射使用 PTE.G。对于这类映射，`sfence.vma va, asid`
    /// 不保证失效 Global translation，必须把第二个操作数编码为寄存器 `x0`。
    /// 这里不能把数值 0 放进普通寄存器代替 `x0`，两者硬件语义不同。
    ///
    /// # Safety
    ///
    /// 必须在 S-mode 下调用；函数返回前会完成本地失效和远端 hart shootdown，
    /// 调用方随后才能释放被解除映射的物理页或页表页。
    #[inline]
    pub unsafe fn flush_tlb_global(vaddr: Option<VirtAddr>) {
        unsafe { Self::flush_tlb_global_local(vaddr) };
        crate::riscv64::smp::remote_sfence_vma(None, vaddr.map(VirtAddr::as_usize));
    }

    /// 只在当前 hart 失效 Global translation。
    ///
    /// 该入口供需要把多个本地地址失效合并为一次 SBI range RFENCE 的架构内部
    /// 路径使用。调用者必须自行保证所有相关远端 hart 在物理页复用前完成失效。
    ///
    /// # Safety
    ///
    /// 必须在 S-mode 下调用。
    #[inline]
    pub(crate) unsafe fn flush_tlb_global_local(vaddr: Option<VirtAddr>) {
        unsafe {
            if let Some(addr) = vaddr {
                core::arch::asm!(
                    "sfence.vma {va}, zero",
                    va = in(reg) addr.as_usize(),
                    options(nostack)
                );
            } else {
                core::arch::asm!("sfence.vma zero, zero", options(nostack));
            }
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
            Riscv64Paging::activate_with_asid(root, Riscv64Paging::current_asid(), true);
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
