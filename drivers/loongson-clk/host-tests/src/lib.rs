//! LS2X 时钟驱动的宿主侧频率计算测试。

// The harness imports the complete production module but exercises only its pure contracts.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../src/layout.rs"]
mod layout;

#[cfg(test)]
mod tests {
    use super::layout::{
        ClockId, Ls2kClockMmioLayout, Ls2kClockMmioLayoutError, Ls2kClockSnapshot,
        clock_id_from_specifier,
    };

    const CLOCK_BASE: usize = 0x1fe0_0480;

    fn board_snapshot() -> Ls2kClockSnapshot {
        Ls2kClockSnapshot {
            sys0: (40u64 << 32) | (4u64 << 26),
            sys1: 8,
            ddr0: (48u64 << 32) | (4u64 << 26),
            ddr1: 6 | (8u64 << 22) | (12u64 << 44),
            dc0: (40u64 << 32) | (4u64 << 26),
            dc1: 5 | (8u64 << 22),
            pix00: (32u64 << 32) | (4u64 << 26),
            pix01: 8,
            pix10: (30u64 << 32) | (5u64 << 26),
            pix11: 10,
            freq_scale: (7u64 << 20) | (1u64 << 16) | (3u64 << 12) | 7,
        }
    }

    #[test]
    fn one_cell_ids_match_the_ls2k1000_binding() {
        assert_eq!(ClockId::try_from(0), Ok(ClockId::Ref));
        assert_eq!(ClockId::try_from(9), Ok(ClockId::Gmac));
        assert_eq!(ClockId::try_from(10), Ok(ClockId::Sata));
        assert_eq!(ClockId::try_from(11), Ok(ClockId::Usb));
        assert_eq!(ClockId::try_from(12), Ok(ClockId::Apb));
        assert_eq!(ClockId::try_from(13), Ok(ClockId::Spi));
        assert!(ClockId::try_from(15).is_err());
    }

    #[test]
    fn clock_provider_requires_exactly_one_valid_cell() {
        assert_eq!(clock_id_from_specifier(&[12]), Ok(ClockId::Apb));
        assert!(clock_id_from_specifier(&[]).is_err());
        assert!(clock_id_from_specifier(&[12, 0]).is_err());
        assert!(clock_id_from_specifier(&[15]).is_err());
    }

    #[test]
    fn apb_sata_usb_and_spi_rates_follow_the_pll_tree() {
        let snapshot = board_snapshot();

        assert_eq!(snapshot.rate(ClockId::Gmac, 100_000_000), Some(125_000_000));
        assert_eq!(snapshot.rate(ClockId::Apb, 100_000_000), Some(125_000_000));
        assert_eq!(snapshot.rate(ClockId::Sata, 100_000_000), Some(62_500_000));
        assert_eq!(snapshot.rate(ClockId::Usb, 100_000_000), Some(31_250_000));
        assert_eq!(snapshot.rate(ClockId::Spi, 100_000_000), Some(100_000_000));
    }

    #[test]
    fn node_cpu_and_secondary_plls_are_decoded_without_rounding_up() {
        let snapshot = board_snapshot();

        assert_eq!(snapshot.rate(ClockId::Node, 100_000_000), Some(125_000_000));
        assert_eq!(snapshot.rate(ClockId::Cpu, 100_000_000), Some(125_000_000));
        assert_eq!(snapshot.rate(ClockId::Ddr, 100_000_000), Some(200_000_000));
        assert_eq!(snapshot.rate(ClockId::Gpu, 100_000_000), Some(150_000_000));
        assert_eq!(snapshot.rate(ClockId::Hda, 100_000_000), Some(100_000_000));
        assert_eq!(snapshot.rate(ClockId::Dc, 100_000_000), Some(200_000_000));
        assert_eq!(snapshot.rate(ClockId::Pix0, 100_000_000), Some(100_000_000));
        assert_eq!(snapshot.rate(ClockId::Pix1, 100_000_000), Some(60_000_000));
    }

    #[test]
    fn zero_divisors_and_zero_parent_rate_are_rejected() {
        let empty = Ls2kClockSnapshot::default();

        assert_eq!(empty.rate(ClockId::Apb, 100_000_000), None);
        assert_eq!(board_snapshot().rate(ClockId::Apb, 0), None);
    }

    #[test]
    fn clock_registers_are_relative_to_the_sys0_dt_address() {
        let registers = Ls2kClockMmioLayout::new(CLOCK_BASE, 1).unwrap().registers();

        assert_eq!(registers.sys0, 0x1fe0_0480);
        assert_eq!(registers.sys1, 0x1fe0_0488);
        assert_eq!(registers.ddr0, 0x1fe0_0490);
        assert_eq!(registers.ddr1, 0x1fe0_0498);
        assert_eq!(registers.dc0, 0x1fe0_04a0);
        assert_eq!(registers.dc1, 0x1fe0_04a8);
        assert_eq!(registers.pix00, 0x1fe0_04b0);
        assert_eq!(registers.pix01, 0x1fe0_04b8);
        assert_eq!(registers.pix10, 0x1fe0_04c0);
        assert_eq!(registers.pix11, 0x1fe0_04c8);
        assert_eq!(registers.freq_scale, 0x1fe0_04d0);
    }

    #[test]
    fn only_the_known_one_byte_firmware_window_or_a_full_window_is_accepted() {
        assert!(Ls2kClockMmioLayout::new(CLOCK_BASE, 1).is_ok());
        assert!(Ls2kClockMmioLayout::new(CLOCK_BASE, 0x58).is_ok());
        assert_eq!(
            Ls2kClockMmioLayout::new(CLOCK_BASE, 0),
            Err(Ls2kClockMmioLayoutError::WindowTooSmall)
        );
        assert_eq!(
            Ls2kClockMmioLayout::new(CLOCK_BASE, 0x50),
            Err(Ls2kClockMmioLayoutError::WindowTooSmall)
        );
    }

    #[test]
    fn clock_window_rejects_unaligned_and_overflowing_addresses() {
        assert_eq!(
            Ls2kClockMmioLayout::new(CLOCK_BASE + 1, 1),
            Err(Ls2kClockMmioLayoutError::Unaligned)
        );
        assert_eq!(
            Ls2kClockMmioLayout::new(usize::MAX - 0x4f, 1),
            Err(Ls2kClockMmioLayoutError::AddressOverflow)
        );
    }
}
