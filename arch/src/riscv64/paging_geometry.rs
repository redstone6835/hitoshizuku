/// RISC-V64 内核支持的硬件分页模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RiscvPagingMode {
    Sv39,
    Sv48,
}

/// 分页模式只允许在早期 Sv39 之后完成一次最终选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PagingModeState {
    EarlySv39 = 0,
    FinalSv39 = 1,
    FinalSv48 = 2,
}

impl PagingModeState {
    pub(crate) const fn finalize(self, mode: RiscvPagingMode) -> Option<Self> {
        if !matches!(self, Self::EarlySv39) {
            return None;
        }
        Some(match mode {
            RiscvPagingMode::Sv39 => Self::FinalSv39,
            RiscvPagingMode::Sv48 => Self::FinalSv48,
        })
    }

    pub(crate) const fn mode(self) -> RiscvPagingMode {
        match self {
            Self::EarlySv39 | Self::FinalSv39 => RiscvPagingMode::Sv39,
            Self::FinalSv48 => RiscvPagingMode::Sv48,
        }
    }

    pub(crate) const fn is_final(self) -> bool {
        !matches!(self, Self::EarlySv39)
    }
}

impl RiscvPagingMode {
    pub(crate) const fn from_mmu_type(mmu_type: &str) -> Option<Self> {
        match mmu_type.as_bytes() {
            b"riscv,sv39" => Some(Self::Sv39),
            b"riscv,sv48" | b"riscv,sv57" => Some(Self::Sv48),
            _ => None,
        }
    }

    pub(crate) const fn satp_mode(self) -> usize {
        match self {
            Self::Sv39 => 8usize << 60,
            Self::Sv48 => 9usize << 60,
        }
    }

    pub(crate) const fn levels(self) -> usize {
        match self {
            Self::Sv39 => 3,
            Self::Sv48 => 4,
        }
    }

    pub(crate) const fn user_space_top(self) -> usize {
        match self {
            Self::Sv39 => 1usize << 38,
            Self::Sv48 => 1usize << 47,
        }
    }

    /// 判断地址是否满足当前模式的符号扩展要求。
    pub(crate) const fn is_canonical(self, vaddr: usize) -> bool {
        let sign_bit = match self {
            Self::Sv39 => 38,
            Self::Sv48 => 47,
        };
        let sign = (vaddr >> sign_bit) & 1;
        let upper = vaddr >> (sign_bit + 1);
        let expected_upper = if sign == 0 {
            0
        } else {
            usize::MAX >> (sign_bit + 1)
        };
        upper == expected_upper
    }

    /// 返回从根开始编号的页表索引，非法层级不产生索引。
    pub(crate) const fn level_index(self, vaddr: usize, level: usize) -> Option<usize> {
        if level >= self.levels() {
            return None;
        }
        let shift = 12 + 9 * (self.levels() - 1 - level);
        Some((vaddr >> shift) & 0x1ff)
    }

    /// 返回硬件允许的叶映射大小；Sv48 的顶层不支持 512 GiB 叶。
    pub(crate) const fn leaf_page_size(self, level: usize) -> Option<usize> {
        if level >= self.levels() {
            return None;
        }
        let shift = 12 + 9 * (self.levels() - 1 - level);
        if shift > 30 {
            None
        } else {
            Some(1usize << shift)
        }
    }
}

/// 取所有可用 hart 都支持的最高分页模式。
pub(crate) fn common_paging_mode(
    modes: impl IntoIterator<Item = RiscvPagingMode>,
) -> Option<RiscvPagingMode> {
    modes.into_iter().reduce(|current, mode| {
        if current == RiscvPagingMode::Sv39 || mode == RiscvPagingMode::Sv39 {
            RiscvPagingMode::Sv39
        } else {
            RiscvPagingMode::Sv48
        }
    })
}

/// 推进页表验证游标，并允许最后一个叶映射抵达虚拟地址空间顶端。
pub(crate) fn advance_leaf_cursor(base: usize, size: usize, limit: usize) -> usize {
    base.saturating_add(size).min(limit)
}

#[cfg(test)]
mod tests {
    use super::{PagingModeState, RiscvPagingMode, advance_leaf_cursor, common_paging_mode};

    #[test]
    fn leaf_walker_stops_at_the_virtual_address_space_top() {
        assert_eq!(
            advance_leaf_cursor(0xffff_ffff_c000_0000, 1usize << 30, usize::MAX),
            usize::MAX
        );
    }

    #[test]
    fn sv39_and_sv48_enforce_their_canonical_address_boundaries() {
        assert!(RiscvPagingMode::Sv39.is_canonical(0x0000_003f_ffff_ffff));
        assert!(RiscvPagingMode::Sv39.is_canonical(0xffff_ffc0_0000_0000));
        assert!(!RiscvPagingMode::Sv39.is_canonical(0x0000_0040_0000_0000));
        assert!(!RiscvPagingMode::Sv39.is_canonical(0xffff_ffbf_ffff_ffff));

        assert!(RiscvPagingMode::Sv48.is_canonical(0x0000_7fff_ffff_ffff));
        assert!(RiscvPagingMode::Sv48.is_canonical(0xffff_8000_0000_0000));
        assert!(!RiscvPagingMode::Sv48.is_canonical(0x0000_8000_0000_0000));
        assert!(!RiscvPagingMode::Sv48.is_canonical(0xffff_7fff_ffff_ffff));
    }

    #[test]
    fn paging_modes_report_hardware_levels_and_leaf_sizes() {
        assert_eq!(RiscvPagingMode::Sv39.levels(), 3);
        assert_eq!(RiscvPagingMode::Sv39.leaf_page_size(0), Some(1 << 30));
        assert_eq!(RiscvPagingMode::Sv39.leaf_page_size(1), Some(1 << 21));
        assert_eq!(RiscvPagingMode::Sv39.leaf_page_size(2), Some(1 << 12));
        assert_eq!(RiscvPagingMode::Sv39.leaf_page_size(3), None);

        assert_eq!(RiscvPagingMode::Sv48.levels(), 4);
        assert_eq!(RiscvPagingMode::Sv48.leaf_page_size(0), None);
        assert_eq!(RiscvPagingMode::Sv48.leaf_page_size(1), Some(1 << 30));
        assert_eq!(RiscvPagingMode::Sv48.leaf_page_size(2), Some(1 << 21));
        assert_eq!(RiscvPagingMode::Sv48.leaf_page_size(3), Some(1 << 12));
    }

    #[test]
    fn common_kernel_layout_uses_the_same_one_gibibyte_slot() {
        const KERNEL_IMAGE_VADDR: usize = 0xffff_ffc0_8020_0000;
        assert_eq!(
            RiscvPagingMode::Sv39.level_index(KERNEL_IMAGE_VADDR, 0),
            Some(258)
        );
        assert_eq!(
            RiscvPagingMode::Sv48.level_index(KERNEL_IMAGE_VADDR, 1),
            Some(258)
        );
        assert_eq!(RiscvPagingMode::Sv39.user_space_top(), 1usize << 38);
        assert_eq!(RiscvPagingMode::Sv48.user_space_top(), 1usize << 47);
    }

    #[test]
    fn platform_mode_is_the_intersection_of_all_harts() {
        assert_eq!(
            common_paging_mode([RiscvPagingMode::Sv48, RiscvPagingMode::Sv48]),
            Some(RiscvPagingMode::Sv48)
        );
        assert_eq!(
            common_paging_mode([RiscvPagingMode::Sv48, RiscvPagingMode::Sv39]),
            Some(RiscvPagingMode::Sv39)
        );
        assert_eq!(common_paging_mode([]), None);
    }

    #[test]
    fn device_tree_mmu_types_map_to_the_highest_supported_mode() {
        assert_eq!(
            RiscvPagingMode::from_mmu_type("riscv,sv39"),
            Some(RiscvPagingMode::Sv39)
        );
        assert_eq!(
            RiscvPagingMode::from_mmu_type("riscv,sv48"),
            Some(RiscvPagingMode::Sv48)
        );
        assert_eq!(
            RiscvPagingMode::from_mmu_type("riscv,sv57"),
            Some(RiscvPagingMode::Sv48)
        );
        assert_eq!(RiscvPagingMode::from_mmu_type("riscv,none"), None);
    }

    #[test]
    fn paging_mode_can_only_be_finalized_once() {
        assert_eq!(
            PagingModeState::EarlySv39.finalize(RiscvPagingMode::Sv39),
            Some(PagingModeState::FinalSv39)
        );
        assert_eq!(
            PagingModeState::EarlySv39.finalize(RiscvPagingMode::Sv48),
            Some(PagingModeState::FinalSv48)
        );
        assert_eq!(
            PagingModeState::FinalSv39.finalize(RiscvPagingMode::Sv48),
            None
        );
        assert_eq!(
            PagingModeState::FinalSv48.finalize(RiscvPagingMode::Sv39),
            None
        );
    }
}
