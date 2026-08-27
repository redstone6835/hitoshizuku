//! Loongson 中断控制器驱动的宿主侧寄存器契约测试。

// The harness imports the complete production module but exercises only its pure contracts.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../src/ls2k_icu_layout.rs"]
mod ls2k_icu_layout;

#[cfg(test)]
mod tests {
    use super::ls2k_icu_layout::{Ls2kIcuLayout, Ls2kIcuLayoutError, pending_sources, route_value};

    const CONTROL_BASE: usize = 0x1fe0_1400;
    const ISR_BASE: usize = 0x1fe0_1040;

    fn board_layout() -> Ls2kIcuLayout {
        Ls2kIcuLayout::new(CONTROL_BASE, 0x40, ISR_BASE, 0x10).unwrap()
    }

    #[test]
    fn source_31_and_32_cross_the_hardware_bank_boundary() {
        let layout = board_layout();
        let low = layout.source(31).unwrap();
        let high = layout.source(32).unwrap();

        assert_eq!(low.route, 0x1fe0_141f);
        assert_eq!(low.enable, 0x1fe0_1428);
        assert_eq!(low.bit, 1 << 31);
        assert_eq!(high.route, 0x1fe0_1440);
        assert_eq!(high.enable, 0x1fe0_1468);
        assert_eq!(high.bit, 1);
    }

    #[test]
    fn source_63_uses_the_last_high_bank_bit() {
        let regs = board_layout().source(63).unwrap();

        assert_eq!(regs.route, 0x1fe0_145f);
        assert_eq!(regs.disable, 0x1fe0_146c);
        assert_eq!(regs.polarity, 0x1fe0_1470);
        assert_eq!(regs.edge, 0x1fe0_1474);
        assert_eq!(regs.bounce, 0x1fe0_1478);
        assert_eq!(regs.auto, 0x1fe0_147c);
        assert_eq!(regs.bit, 1 << 31);
        assert!(board_layout().source(64).is_none());
    }

    #[test]
    fn pending_registers_use_per_core_stride() {
        let layout = board_layout();

        assert_eq!(layout.pending(0), Some([0x1fe0_1040, 0x1fe0_1048]));
        assert_eq!(layout.pending(1), Some([0x1fe0_1140, 0x1fe0_1148]));
    }

    #[test]
    fn malformed_windows_and_overflow_are_rejected() {
        assert_eq!(
            Ls2kIcuLayout::new(CONTROL_BASE + 1, 0x40, ISR_BASE, 0x10),
            Err(Ls2kIcuLayoutError::Unaligned)
        );
        assert_eq!(
            Ls2kIcuLayout::new(CONTROL_BASE, 0x3f, ISR_BASE, 0x10),
            Err(Ls2kIcuLayoutError::ControlWindowTooSmall)
        );
        assert_eq!(
            Ls2kIcuLayout::new(CONTROL_BASE, 0x40, ISR_BASE, 0x0f),
            Err(Ls2kIcuLayoutError::IsrWindowTooSmall)
        );
        assert_eq!(
            Ls2kIcuLayout::new(usize::MAX - 0x3f, 0x40, ISR_BASE, 0x10),
            Err(Ls2kIcuLayoutError::AddressOverflow)
        );
    }

    #[test]
    fn pending_words_expand_across_all_64_sources() {
        assert_eq!(
            pending_sources([1 << 31, 1]).collect::<Vec<_>>(),
            vec![31, 32]
        );
        assert_eq!(pending_sources([0, 1 << 31]).collect::<Vec<_>>(), vec![63]);
        assert!(pending_sources([0, 0]).next().is_none());
    }

    #[test]
    fn route_combines_parent_line_and_core_mask() {
        assert_eq!(route_value(1, 0b0001), Some(0x21));
        assert_eq!(route_value(1, 0b0011), Some(0x23));
        assert_eq!(route_value(4, 0b0001), None);
        assert_eq!(route_value(1, 0), None);
        assert_eq!(route_value(1, 0x10), None);
    }
}
