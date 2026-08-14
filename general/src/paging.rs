//! 架构相关的分页操作接口。
//!
//! 该 trait 位于 `hal` 层，隔离分页通用逻辑与具体 ISA 页表格式。
//! 这里使用 `hal` 层 newtype 表达跨层边界上的地址语义，避免把上层
//! crate 的地址类型或裸 `usize` 暴露给架构实现。

use crate::{PhysPageTableRoot, VirtAddr};

pub trait PagingArch {
    type Pte: Copy;
    type Flags: Copy;

    const PAGE_SIZE: usize;
    const LEVELS: usize;
    const ENTRIES_PER_TABLE: usize;

    /// 返回当前硬件页表实际使用的层数。
    ///
    /// 固定分页架构沿用编译期层数；支持启动期选择模式的架构可以覆盖此值。
    fn active_levels() -> usize {
        Self::LEVELS
    }

    fn is_canonical_vaddr(vaddr: usize) -> bool;
    fn level_index(vaddr: usize, level: usize) -> usize;

    fn invalid_pte() -> Self::Pte;
    fn pte_is_valid(pte: Self::Pte) -> bool;
    fn pte_is_leaf(pte: Self::Pte) -> bool;
    fn pte_addr(pte: Self::Pte) -> usize;
    fn pte_flags(pte: Self::Pte) -> Self::Flags;
    fn flags_readable(flags: Self::Flags) -> bool;
    fn flags_writable(flags: Self::Flags) -> bool;
    fn flags_executable(flags: Self::Flags) -> bool;
    fn flags_user_accessible(flags: Self::Flags) -> bool;
    fn flags_global(flags: Self::Flags) -> bool;

    fn make_table_pte(next_table: usize) -> Self::Pte;
    fn is_valid_leaf_perm(read: bool, write: bool, execute: bool, user: bool, global: bool)
    -> bool;
    fn supported_leaf_levels() -> &'static [usize];
    fn leaf_page_size(level: usize) -> Option<usize>;
    fn make_leaf_pte(
        paddr: usize,
        read: bool,
        write: bool,
        execute: bool,
        user: bool,
        global: bool,
    ) -> Self::Pte;
    fn make_leaf_pte_for_level(
        level: usize,
        paddr: usize,
        read: bool,
        write: bool,
        execute: bool,
        user: bool,
        global: bool,
    ) -> Option<Self::Pte>;

    /// 将 PTE 转换为原生位表示（usize），用于写入页表内存。
    ///
    /// 通用页表遍历代码在操作页表项时使用 `*mut usize` 存储 PTE，
    /// 因此需要此转换。架构实现应返回与硬件格式一致的位模式。
    fn pte_to_usize(pte: Self::Pte) -> usize;

    /// 从原生位表示（usize）还原 PTE，用于读取页表内存。
    ///
    /// 此转换是 [`pte_to_usize`](Self::pte_to_usize) 的逆操作。
    fn pte_from_usize(bits: usize) -> Self::Pte;

    /// .
    ///
    /// # Safety
    ///
    /// .
    unsafe fn activate(root: PhysPageTableRoot);

    /// .
    ///
    /// # Safety
    ///
    /// .
    unsafe fn flush_tlb(vaddr: Option<VirtAddr>);
}

#[cfg(test)]
mod tests {
    use super::PagingArch;
    use crate::{PhysPageTableRoot, VirtAddr};

    struct FixedDepth;
    struct RuntimeDepth;

    macro_rules! impl_test_paging {
        ($paging:ty, $active_levels:expr) => {
            impl PagingArch for $paging {
                type Pte = usize;
                type Flags = usize;

                const PAGE_SIZE: usize = 4096;
                const LEVELS: usize = 4;
                const ENTRIES_PER_TABLE: usize = 512;

                fn active_levels() -> usize {
                    $active_levels
                }

                fn is_canonical_vaddr(_: usize) -> bool {
                    true
                }
                fn level_index(_: usize, _: usize) -> usize {
                    0
                }
                fn invalid_pte() -> usize {
                    0
                }
                fn pte_is_valid(pte: usize) -> bool {
                    pte != 0
                }
                fn pte_is_leaf(_: usize) -> bool {
                    false
                }
                fn pte_addr(_: usize) -> usize {
                    0
                }
                fn pte_flags(_: usize) -> usize {
                    0
                }
                fn flags_readable(_: usize) -> bool {
                    false
                }
                fn flags_writable(_: usize) -> bool {
                    false
                }
                fn flags_executable(_: usize) -> bool {
                    false
                }
                fn flags_user_accessible(_: usize) -> bool {
                    false
                }
                fn flags_global(_: usize) -> bool {
                    false
                }
                fn make_table_pte(_: usize) -> usize {
                    0
                }
                fn is_valid_leaf_perm(_: bool, _: bool, _: bool, _: bool, _: bool) -> bool {
                    true
                }
                fn supported_leaf_levels() -> &'static [usize] {
                    &[]
                }
                fn leaf_page_size(_: usize) -> Option<usize> {
                    None
                }
                fn make_leaf_pte(_: usize, _: bool, _: bool, _: bool, _: bool, _: bool) -> usize {
                    0
                }
                fn make_leaf_pte_for_level(
                    _: usize,
                    _: usize,
                    _: bool,
                    _: bool,
                    _: bool,
                    _: bool,
                    _: bool,
                ) -> Option<usize> {
                    None
                }
                fn pte_to_usize(pte: usize) -> usize {
                    pte
                }
                fn pte_from_usize(bits: usize) -> usize {
                    bits
                }
                unsafe fn activate(_: PhysPageTableRoot) {}
                unsafe fn flush_tlb(_: Option<VirtAddr>) {}
            }
        };
    }

    impl_test_paging!(FixedDepth, 4);
    impl_test_paging!(RuntimeDepth, 3);

    impl RuntimeDepth {
        fn reported_depth() -> usize {
            <Self as PagingArch>::active_levels()
        }
    }

    #[test]
    fn fixed_architecture_defaults_to_maximum_depth() {
        assert_eq!(<FixedDepth as PagingArch>::active_levels(), 4);
    }

    #[test]
    fn architecture_can_report_a_smaller_active_depth() {
        assert_eq!(RuntimeDepth::reported_depth(), 3);
    }
}
