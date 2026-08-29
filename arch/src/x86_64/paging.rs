//! x86_64 页表根、TLB 和页表项辅助。

use core::sync::atomic::{AtomicUsize, Ordering};

use general::{PagingArch, PhysPageTableRoot, VirtAddr};

/// x86_64 页表项位定义（四级/五级页表通用）。
pub const PTE_PRESENT: u64 = 1 << 0;
pub const PTE_WRITABLE: u64 = 1 << 1;
pub const PTE_USER: u64 = 1 << 2;
pub const PTE_WRITE_THROUGH: u64 = 1 << 3;
pub const PTE_CACHE_DISABLE: u64 = 1 << 4;
pub const PTE_ACCESSED: u64 = 1 << 5;
pub const PTE_DIRTY: u64 = 1 << 6;
pub const PTE_HUGE: u64 = 1 << 7;
pub const PTE_GLOBAL: u64 = 1 << 8;
/// 软件叶标志。x86 的 4-KiB PTE 与非叶目录项都使用 PS=0，
/// 因而需要占用硬件保留给软件的 bit 9 来让通用 walker 区分二者。
/// 硬件忽略该位，且它不参与地址/权限编码。
pub const PTE_SOFT_LEAF: u64 = 1 << 9;
/// 软件只读语义标志。x86 硬件本身没有只读 present 位之外的读权限位，
/// 该位供通用权限检查保留；写权限仍由硬件 PTE_WRITABLE 控制。
pub const PTE_SOFT_READ: u64 = 1 << 10;
pub const PTE_NO_EXECUTE: u64 = 1 << 63;
pub const PTE_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct PageTableEntry(pub u64);

impl PageTableEntry {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn new(physical: u64, flags: u64) -> Self {
        Self((physical & PTE_ADDR_MASK) | flags)
    }

    pub const fn address(self) -> u64 {
        self.0 & PTE_ADDR_MASK
    }

    pub const fn flags(self) -> u64 {
        self.0 & !PTE_ADDR_MASK
    }

    pub const fn is_present(self) -> bool {
        self.0 & PTE_PRESENT != 0
    }

    pub const fn is_huge(self) -> bool {
        self.0 & PTE_HUGE != 0
    }

    pub const fn is_4k_leaf(self) -> bool {
        self.0 & PTE_SOFT_LEAF != 0
    }
}

static CR3_MIRROR: AtomicUsize = AtomicUsize::new(0);

/// 读取当前页表根（CR3 的物理地址与 PCID 字段）。
pub fn read_cr3() -> usize {
    #[cfg(target_os = "none")]
    {
        let value: usize;
        unsafe {
            // Reading CR3 is an architectural page-table boundary.  Retain
            // the implicit memory clobber so table accesses are not moved
            // across the root read by the compiler.
            core::arch::asm!("mov {}, cr3", out(reg) value, options(nostack));
        }
        value
    }
    #[cfg(not(target_os = "none"))]
    {
        CR3_MIRROR.load(Ordering::Acquire)
    }
}

/// 写入页表根。调用方负责确保新根指向有效、已对齐的 PML4/PML5。
pub unsafe fn write_cr3(value: usize) {
    #[cfg(target_os = "none")]
    unsafe {
        // Loading CR3 changes the translation context.  Linux's
        // `native_write_cr3()` uses a `"memory"` clobber so page-table writes
        // cannot be moved across the switch by the compiler.
        core::arch::asm!("mov cr3, {}", in(reg) value, options(nostack));
    }
    CR3_MIRROR.store(value, Ordering::Release);
}

/// 使单个线性地址的 TLB 项失效。
pub unsafe fn invlpg(address: usize) {
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags));
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = address;
    }
}

/// 通过重写 CR3 刷新当前地址空间的非全局 TLB。
pub unsafe fn flush_tlb() {
    // CR3 bit 63 is the PCID no-flush hint.  Rewriting it unchanged would
    // explicitly retain translations instead of flushing the non-global TLB.
    // Preserve the root/PCID fields while clearing only that hint.
    let root = read_cr3() & !(1usize << 63);
    unsafe { write_cr3(root) };
}

/// 判断地址是否符合当前 x86_64 的 canonical 规则。
///
/// `la57=false` 使用 48 位虚拟地址，`la57=true` 使用 57 位虚拟地址。
pub const fn is_canonical(address: u64, la57: bool) -> bool {
    let bits = if la57 { 57 } else { 48 };
    let sign = (address >> (bits - 1)) & 1;
    let upper = address >> bits;
    if sign == 0 {
        upper == 0
    } else {
        upper == ((1u64 << (64 - bits)) - 1)
    }
}

pub const fn page_offset(address: usize) -> usize {
    address & 0xfff
}

pub const fn page_align_down(address: usize) -> usize {
    address & !0xfff
}

pub const fn page_align_up(address: usize) -> Option<usize> {
    match address.checked_add(0xfff) {
        Some(value) => Some(value & !0xfff),
        None => None,
    }
}

/// x86_64 四级页表实现。
///
/// level 0..3 分别对应 PML4/PDPT/PD/PT；目录级 PS 叶覆盖 1 GiB 或
/// 2 MiB，末级 4 KiB 叶用软件 bit 9 标记。这样 `general::page_walk`
/// 不需要知道 x86 的“末级非叶同编码”特殊性。
pub struct X86_64Paging;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct X86_64Flags(pub u64);

const X86_LEVELS: usize = 4;
const X86_ENTRIES: usize = 512;
const PAGE_SHIFT: usize = 12;
const SUPPORTED_LEAF_LEVELS: [usize; 3] = [1, 2, 3];
const PHYS_ADDR_MASK: u64 = PTE_ADDR_MASK;

impl X86_64Paging {
    pub const fn level_page_size(level: usize) -> Option<usize> {
        if level >= X86_LEVELS {
            return None;
        }
        Some(1usize << (PAGE_SHIFT + 9 * (X86_LEVELS - 1 - level)))
    }

    pub const fn level_index_const(vaddr: usize, level: usize) -> usize {
        let shift = PAGE_SHIFT + 9 * (X86_LEVELS - 1 - level);
        (vaddr >> shift) & 0x1ff
    }

    #[inline]
    pub fn set_pcid(value: usize, pcid: u16, no_flush: bool) -> usize {
        // CR3 bit 63 is the PCID no-flush hint, not part of the page-table
        // root.  Clear it before applying the caller's requested value so a
        // previous context's hint cannot leak into a normal flush switch.
        let root = value & !(0xfff | (1usize << 63));
        root | usize::from(pcid & 0x0fff) | if no_flush { 1usize << 63 } else { 0 }
    }

    #[inline]
    pub fn physical_address_valid(address: usize) -> bool {
        (address as u64) & !PHYS_ADDR_MASK == 0 && address & 0xfff == 0
    }
}

impl PagingArch for X86_64Paging {
    type Pte = PageTableEntry;
    type Flags = X86_64Flags;

    const PAGE_SIZE: usize = 4096;
    const LEVELS: usize = X86_LEVELS;
    const ENTRIES_PER_TABLE: usize = X86_ENTRIES;

    fn is_canonical_vaddr(vaddr: usize) -> bool {
        is_canonical(vaddr as u64, false)
    }

    fn level_index(vaddr: usize, level: usize) -> usize {
        Self::level_index_const(vaddr, level)
    }

    fn invalid_pte() -> Self::Pte {
        PageTableEntry::empty()
    }

    fn pte_is_valid(pte: Self::Pte) -> bool {
        pte.is_present()
    }

    fn pte_is_leaf(pte: Self::Pte) -> bool {
        pte.is_present() && (pte.is_huge() || pte.is_4k_leaf())
    }

    fn pte_addr(pte: Self::Pte) -> usize {
        (pte.address() & PHYS_ADDR_MASK) as usize
    }

    fn pte_flags(pte: Self::Pte) -> Self::Flags {
        X86_64Flags(pte.0 & !PHYS_ADDR_MASK)
    }

    fn flags_readable(flags: Self::Flags) -> bool {
        flags.0 & PTE_SOFT_READ != 0
    }

    fn flags_writable(flags: Self::Flags) -> bool {
        flags.0 & PTE_WRITABLE != 0
    }

    fn flags_executable(flags: Self::Flags) -> bool {
        flags.0 & PTE_NO_EXECUTE == 0
    }

    fn flags_user_accessible(flags: Self::Flags) -> bool {
        flags.0 & PTE_USER != 0
    }

    fn flags_global(flags: Self::Flags) -> bool {
        flags.0 & PTE_GLOBAL != 0
    }

    fn make_table_pte(next_table: usize) -> Self::Pte {
        PageTableEntry::new(next_table as u64, PTE_PRESENT | PTE_WRITABLE | PTE_USER)
    }

    fn is_valid_leaf_perm(
        read: bool,
        write: bool,
        _execute: bool,
        user: bool,
        global: bool,
    ) -> bool {
        // User and global are independent in x86, but a global user mapping is
        // almost certainly a caller error and leaks an address space across CR3.
        !(global && user) && (read || write)
    }

    fn supported_leaf_levels() -> &'static [usize] {
        &SUPPORTED_LEAF_LEVELS
    }

    fn leaf_page_size(level: usize) -> Option<usize> {
        Self::level_page_size(level).filter(|_| SUPPORTED_LEAF_LEVELS.contains(&level))
    }

    fn make_leaf_pte(
        paddr: usize,
        read: bool,
        write: bool,
        execute: bool,
        user: bool,
        global: bool,
    ) -> Self::Pte {
        Self::make_leaf_pte_for_level(3, paddr, read, write, execute, user, global)
            .unwrap_or_else(|| PageTableEntry::empty())
    }

    fn make_leaf_pte_for_level(
        level: usize,
        paddr: usize,
        read: bool,
        write: bool,
        execute: bool,
        user: bool,
        global: bool,
    ) -> Option<Self::Pte> {
        let page_size = Self::leaf_page_size(level)?;
        if paddr & (page_size - 1) != 0
            || !Self::is_valid_leaf_perm(read, write, execute, user, global)
        {
            return None;
        }
        let mut flags = PTE_PRESENT | PTE_SOFT_READ;
        if write {
            flags |= PTE_WRITABLE;
        }
        if user {
            flags |= PTE_USER;
        }
        if global {
            flags |= PTE_GLOBAL;
        }
        if !execute {
            flags |= PTE_NO_EXECUTE;
        }
        if level < 3 {
            flags |= PTE_HUGE;
        } else {
            flags |= PTE_SOFT_LEAF;
        }
        Some(PageTableEntry::new(paddr as u64, flags))
    }

    fn pte_to_usize(pte: Self::Pte) -> usize {
        pte.0 as usize
    }

    fn pte_from_usize(bits: usize) -> Self::Pte {
        PageTableEntry(bits as u64)
    }

    unsafe fn activate(root: PhysPageTableRoot) {
        let value = root.as_usize();
        assert!(value & 0xfff == 0, "x86 CR3 root must be 4K aligned");
        unsafe { write_cr3(value) };
    }

    unsafe fn flush_tlb(vaddr: Option<VirtAddr>) {
        match vaddr {
            Some(address) => unsafe { invlpg(address.as_usize()) },
            None => unsafe { flush_tlb() },
        }
    }
}

#[cfg(test)]
mod paging_trait_tests {
    use super::*;
    use general::PagingArch;

    #[test]
    fn four_level_indices_and_leaf_markers_are_stable() {
        let va = 0x0000_1234_5678_9000usize;
        assert_eq!(X86_64Paging::level_index(va, 3), 0x189);
        let leaf =
            X86_64Paging::make_leaf_pte_for_level(3, 0x20_0000, true, true, false, true, false)
                .unwrap();
        assert!(X86_64Paging::pte_is_leaf(leaf));
        assert!(leaf.0 & PTE_SOFT_LEAF != 0);
        let table = X86_64Paging::make_table_pte(0x30_0000);
        assert!(!X86_64Paging::pte_is_leaf(table));
    }

    #[test]
    fn permission_policy_rejects_global_user_aliases() {
        assert!(!X86_64Paging::is_valid_leaf_perm(
            true, false, false, true, true
        ));
        assert!(X86_64Paging::is_valid_leaf_perm(
            true, false, false, true, false
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_address_boundaries() {
        assert!(is_canonical(0x0000_7fff_ffff_ffff, false));
        assert!(is_canonical(0xffff_8000_0000_0000, false));
        assert!(!is_canonical(0x0000_8000_0000_0000, false));
        assert!(is_canonical(0x00ff_ffff_ffff_ffff, true));
    }

    #[test]
    fn pte_roundtrip() {
        let pte = PageTableEntry::new(0x1234_5000, PTE_PRESENT | PTE_USER);
        assert_eq!(pte.address(), 0x1234_5000);
        assert!(pte.is_present());
        assert_eq!(pte.flags() & PTE_USER, PTE_USER);
    }

    #[test]
    fn pcid_replaces_the_no_flush_hint() {
        let root = 0x0000_1234_5678_9000usize;
        assert_eq!(
            X86_64Paging::set_pcid(root | (1usize << 63), 0x1234, false),
            root | 0x234
        );
        assert_eq!(
            X86_64Paging::set_pcid(root, 0x1234, true),
            root | 0x234 | (1usize << 63)
        );
    }
}
