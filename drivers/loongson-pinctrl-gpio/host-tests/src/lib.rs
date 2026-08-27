//! LS2K1000 引脚复用和 GPIO 寄存器模型的宿主侧测试。

extern crate alloc;

// The harness imports complete production modules but exercises only their pure contracts.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../src/gpio.rs"]
mod gpio;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../src/pinctrl.rs"]
mod pinctrl;

#[cfg(test)]
mod tests {
    use super::gpio::{
        GpioError, GpioIrqMap, GpioLayout, GpioLineAllocator, GpioOffsets, GpioSpecifier,
        RegisterUpdate,
    };
    use super::pinctrl::{
        MuxUpdate, PinFunction, PinGroup, PinctrlError, PinctrlMmioLayout, decode_named_state,
        decode_state, merge_states,
    };

    const PINCTRL_BASE: usize = 0x1fe0_0420;
    const GPIO_BASE: usize = 0x1fe0_0500;

    fn update(offset: usize, mask: u32, value: u32) -> MuxUpdate {
        MuxUpdate {
            offset,
            mask,
            value,
        }
    }

    #[test]
    fn all_documented_group_and_function_names_are_strictly_parsed() {
        let groups = [
            ("sata_led", PinGroup::SataLed),
            ("gmac1", PinGroup::Gmac1),
            ("dvo0_lio_uart", PinGroup::Dvo0LioUart),
            ("uart1", PinGroup::Uart1),
            ("uart2", PinGroup::Uart2),
            ("dvo1_camera", PinGroup::Dvo1Camera),
            ("can0", PinGroup::Can0),
            ("can1", PinGroup::Can1),
            ("hda_i2s", PinGroup::HdaI2s),
            ("i2c0", PinGroup::I2c0),
            ("i2c1", PinGroup::I2c1),
            ("uart0", PinGroup::Uart0),
            ("nand", PinGroup::Nand),
            ("pwm0", PinGroup::Pwm0),
            ("pwm1", PinGroup::Pwm1),
            ("pwm2", PinGroup::Pwm2),
            ("pwm3", PinGroup::Pwm3),
            ("sdio", PinGroup::Sdio),
        ];
        for (name, group) in groups {
            assert_eq!(PinGroup::parse(name), Some(group));
        }
        assert_eq!(PinGroup::parse("unknown"), None);

        let functions = [
            ("sata_led", PinFunction::SataLed),
            ("gpio", PinFunction::Gpio),
            ("gmac1", PinFunction::Gmac1),
            ("dvo0", PinFunction::Dvo0),
            ("lio", PinFunction::Lio),
            ("uart1_4", PinFunction::Uart1Mode4),
            ("uart1_2", PinFunction::Uart1Mode2),
            ("uart1_1", PinFunction::Uart1Mode1),
            ("uart2_4", PinFunction::Uart2Mode4),
            ("uart2_2", PinFunction::Uart2Mode2),
            ("uart2_1", PinFunction::Uart2Mode1),
            ("dvo1", PinFunction::Dvo1),
            ("camera", PinFunction::Camera),
            ("can0", PinFunction::Can0),
            ("can1", PinFunction::Can1),
            ("hda", PinFunction::Hda),
            ("i2s", PinFunction::I2s),
            ("i2c0", PinFunction::I2c0),
            ("i2c1", PinFunction::I2c1),
            ("uart0_4", PinFunction::Uart0Mode4),
            ("uart0_2", PinFunction::Uart0Mode2),
            ("uart0_1", PinFunction::Uart0Mode1),
            ("nand", PinFunction::Nand),
            ("pwm0", PinFunction::Pwm0),
            ("pwm1", PinFunction::Pwm1),
            ("pwm2", PinFunction::Pwm2),
            ("pwm3", PinFunction::Pwm3),
            ("sdio", PinFunction::Sdio),
        ];
        for (name, function) in functions {
            assert_eq!(PinFunction::parse(name), Some(function));
        }
        assert_eq!(PinFunction::parse("unknown"), None);
    }

    #[test]
    fn simple_peripherals_change_only_their_documented_mux_bit() {
        let cases = [
            (PinGroup::Gmac1, PinFunction::Gmac1, 3),
            (PinGroup::SataLed, PinFunction::SataLed, 8),
            (PinGroup::I2c0, PinFunction::I2c0, 10),
            (PinGroup::I2c1, PinFunction::I2c1, 11),
            (PinGroup::Pwm0, PinFunction::Pwm0, 12),
            (PinGroup::Pwm3, PinFunction::Pwm3, 15),
            (PinGroup::Can0, PinFunction::Can0, 16),
            (PinGroup::Can1, PinFunction::Can1, 17),
            (PinGroup::Sdio, PinFunction::Sdio, 20),
        ];

        for (group, function, bit) in cases {
            assert_eq!(
                decode_state(group, function),
                Ok(vec![update(0, 1 << bit, 1 << bit)])
            );
        }
    }

    #[test]
    fn mux_update_preserves_every_bit_outside_its_mask() {
        let update = update(0x08, 0x0000_00f0, 0x0000_0030);
        assert_eq!(update.apply(0xa5a5_a5a5), 0xa5a5_a535);
    }

    #[test]
    fn gpio_function_clears_only_muxable_peripheral_fields() {
        assert_eq!(
            decode_state(PinGroup::SataLed, PinFunction::Gpio),
            Ok(vec![update(0, 1 << 8, 0)])
        );
        assert_eq!(
            decode_state(PinGroup::HdaI2s, PinFunction::Gpio),
            Ok(vec![update(0, 1 << 6, 0), update(0, 1 << 4, 0)])
        );
        assert_eq!(
            decode_state(PinGroup::Gmac1, PinFunction::Gpio),
            Err(PinctrlError::InvalidCombination)
        );
        assert_eq!(
            decode_state(PinGroup::Uart0, PinFunction::Gpio),
            Err(PinctrlError::InvalidCombination)
        );
    }

    #[test]
    fn uart_modes_preserve_the_other_uart_bank() {
        assert_eq!(
            decode_state(PinGroup::Uart1, PinFunction::Uart1Mode4),
            Ok(vec![
                update(0x10, 1 << 1, 0),
                update(0x08, 1 << 12, 1 << 12),
                update(0x00, 1 << 7, 0),
                update(0x08, 0xf << 4, 0xf << 4),
            ])
        );
        assert_eq!(
            decode_state(PinGroup::Uart2, PinFunction::Uart2Mode2),
            Ok(vec![
                update(0x10, 1 << 1, 0),
                update(0x08, 1 << 13, 1 << 13),
                update(0x00, 1 << 7, 0),
                update(0x08, 0xf << 8, 0x3 << 8),
            ])
        );
    }

    #[test]
    fn display_lio_audio_and_uart0_fields_match_the_mux_table() {
        assert_eq!(
            decode_state(PinGroup::Dvo0LioUart, PinFunction::Dvo0),
            Ok(vec![
                update(0x10, 1 << 1, 1 << 1),
                update(0x08, 1 << 12, 0),
                update(0x08, 1 << 13, 0),
                update(0x00, 1 << 7, 0),
            ])
        );
        assert_eq!(
            decode_state(PinGroup::Dvo0LioUart, PinFunction::Lio),
            Ok(vec![
                update(0x10, 1 << 1, 0),
                update(0x08, 1 << 12, 0),
                update(0x08, 1 << 13, 0),
                update(0x00, 1 << 7, 1 << 7),
            ])
        );
        assert_eq!(
            decode_state(PinGroup::Dvo1Camera, PinFunction::Camera),
            Ok(vec![update(0x10, 1 << 4, 0), update(0x10, 1 << 5, 1 << 5)])
        );
        assert_eq!(
            decode_state(PinGroup::HdaI2s, PinFunction::Hda),
            Ok(vec![update(0, 1 << 6, 0), update(0, 1 << 4, 1 << 4)])
        );
        assert_eq!(
            decode_state(PinGroup::Uart0, PinFunction::Uart0Mode1),
            Ok(vec![update(0x08, 0xf, 1)])
        );
    }

    #[test]
    fn incompatible_and_conflicting_states_are_rejected() {
        assert_eq!(
            decode_state(PinGroup::Can0, PinFunction::Can1),
            Err(PinctrlError::InvalidCombination)
        );
        assert_eq!(
            merge_states(&[
                (PinGroup::Dvo0LioUart, PinFunction::Dvo0),
                (PinGroup::Uart1, PinFunction::Uart1Mode4),
            ]),
            Err(PinctrlError::ConflictingUpdates)
        );
        assert!(
            merge_states(&[
                (PinGroup::Uart1, PinFunction::Uart1Mode4),
                (PinGroup::Uart2, PinFunction::Uart2Mode4),
            ])
            .is_ok()
        );
    }

    #[test]
    fn named_dtb_state_is_decoded_through_the_same_strict_table() {
        assert_eq!(
            decode_named_state(&["uart1"], "uart1_2"),
            Ok(vec![
                update(0x10, 1 << 1, 0),
                update(0x08, (1 << 12) | (0xf << 4), (1 << 12) | (0x3 << 4)),
                update(0x00, 1 << 7, 0),
            ])
        );
        assert_eq!(
            decode_named_state(&["uart1", "uart2"], "uart1_4"),
            Err(PinctrlError::InvalidCombination)
        );
        assert_eq!(
            decode_named_state(&[], "gmac1"),
            Err(PinctrlError::EmptyState)
        );
        assert_eq!(
            decode_named_state(&["unknown"], "gmac1"),
            Err(PinctrlError::UnknownGroup)
        );
        assert_eq!(
            decode_named_state(&["gmac1"], "unknown"),
            Err(PinctrlError::UnknownFunction)
        );
    }

    #[test]
    fn pinctrl_window_checks_alignment_size_and_address_overflow() {
        let layout = PinctrlMmioLayout::new(PINCTRL_BASE, 0x18).unwrap();
        assert_eq!(layout.address(update(0x10, 1, 1)), Ok(PINCTRL_BASE + 0x10));
        assert_eq!(
            PinctrlMmioLayout::new(PINCTRL_BASE + 1, 0x18),
            Err(PinctrlError::UnalignedWindow)
        );
        assert_eq!(
            PinctrlMmioLayout::new(PINCTRL_BASE, 0x14),
            Err(PinctrlError::WindowTooSmall)
        );
        assert_eq!(
            PinctrlMmioLayout::new(usize::MAX - 0x0f, 0x18),
            Err(PinctrlError::AddressOverflow)
        );
        assert_eq!(
            layout.address(update(0x18, 1, 1)),
            Err(PinctrlError::UpdateOutsideWindow)
        );
    }

    fn board_gpio_layout() -> GpioLayout {
        GpioLayout::new(
            GPIO_BASE,
            0x38,
            64,
            GpioOffsets {
                direction: 0,
                output: 0x10,
                input: 0x20,
                interrupt: 0x30,
            },
            0,
        )
        .unwrap()
    }

    #[test]
    fn all_64_gpio_lines_map_to_the_expected_64_bit_registers() {
        let layout = board_gpio_layout();
        let first = layout.line(0).unwrap();
        let last = layout.line(63).unwrap();

        assert_eq!(first.direction_register, GPIO_BASE);
        assert_eq!(layout.ngpios(), 64);
        assert_eq!(layout.size(), 0x38);
        assert_eq!(first.output_register, GPIO_BASE + 0x10);
        assert_eq!(first.input_register, GPIO_BASE + 0x20);
        assert_eq!(first.interrupt_register, GPIO_BASE + 0x30);
        assert_eq!(first.direction_mask, 1);
        assert_eq!(last.direction_mask, 1u64 << 63);
        assert_eq!(last.input_mask, 1u64 << 63);
        assert_eq!(layout.line(64), Err(GpioError::LineOutOfRange));
    }

    #[test]
    fn output_configuration_sets_level_before_enabling_output_drive() {
        let layout = board_gpio_layout();
        assert_eq!(
            layout.output_sequence(7, true),
            Ok([
                RegisterUpdate {
                    address: GPIO_BASE + 0x10,
                    clear_mask: 0,
                    set_mask: 1 << 7,
                },
                RegisterUpdate {
                    address: GPIO_BASE,
                    clear_mask: 1 << 7,
                    set_mask: 0,
                },
            ])
        );
        assert_eq!(
            layout.output_sequence(7, false).unwrap()[0],
            RegisterUpdate {
                address: GPIO_BASE + 0x10,
                clear_mask: 1 << 7,
                set_mask: 0,
            }
        );
        assert_eq!(
            layout.input_update(7),
            Ok(RegisterUpdate {
                address: GPIO_BASE,
                clear_mask: 0,
                set_mask: 1 << 7,
            })
        );
        assert_eq!(
            layout.interrupt_update(7, true),
            Ok(RegisterUpdate {
                address: GPIO_BASE + 0x30,
                clear_mask: 0,
                set_mask: 1 << 7,
            })
        );
    }

    #[test]
    fn gpio_specifier_applies_active_low_and_rejects_unknown_flags() {
        let normal = GpioSpecifier::decode(&[12, 0], 64).unwrap();
        let active_low = GpioSpecifier::decode(&[12, 1], 64).unwrap();

        assert!(!normal.active_low);
        assert!(active_low.active_low);
        assert!(normal.physical_level(true));
        assert!(!active_low.physical_level(true));
        assert!(!active_low.logical_level(true));
        assert_eq!(
            GpioSpecifier::decode(&[64, 0], 64),
            Err(GpioError::LineOutOfRange)
        );
        assert_eq!(
            GpioSpecifier::decode(&[0, 2], 64),
            Err(GpioError::UnsupportedFlags)
        );
        assert_eq!(
            GpioSpecifier::decode(&[0], 64),
            Err(GpioError::InvalidSpecifier)
        );
    }

    #[test]
    fn gpio_irq_map_preserves_the_board_shared_sources() {
        let mut sources = [0x3au32; 64];
        sources[0] = 0x3c;
        sources[1] = 0x3d;
        sources[2] = 0x3e;
        sources[3] = 0x3f;
        for source in &mut sources[31..] {
            *source = 0x3b;
        }
        let map = GpioIrqMap::new(64, &sources, true).unwrap();

        assert_eq!(map.source_for_line(0), Ok(0x3c));
        assert_eq!(map.source_for_line(16), Ok(0x3a));
        assert_eq!(map.source_for_line(30), Ok(0x3a));
        assert_eq!(map.source_for_line(31), Ok(0x3b));
        assert_eq!(map.source_for_line(63), Ok(0x3b));
        assert_eq!(map.source_for_line(64), Err(GpioError::LineOutOfRange));
    }

    #[test]
    fn partial_gpio_irq_map_keeps_unmapped_lines_available_for_io() {
        let sources = [0x3bu32; 60];
        let board_map = GpioIrqMap::new(64, &sources, true).unwrap();
        assert!(board_map.has_source_for_line(59));
        assert!(!board_map.has_source_for_line(60));
        assert!(!board_map.has_source_for_line(64));
        assert_eq!(board_map.source_for_line(59), Ok(0x3b));
        assert_eq!(
            board_map.source_for_line(60),
            Err(GpioError::InterruptsUnsupported)
        );
        assert_eq!(
            board_map.source_for_line(64),
            Err(GpioError::LineOutOfRange)
        );
        assert_eq!(
            board_gpio_layout().output_sequence(63, true),
            Ok([
                RegisterUpdate {
                    address: GPIO_BASE + 0x10,
                    clear_mask: 0,
                    set_mask: 1u64 << 63,
                },
                RegisterUpdate {
                    address: GPIO_BASE,
                    clear_mask: 1u64 << 63,
                    set_mask: 0,
                },
            ])
        );
    }

    #[test]
    fn gpio_irq_map_accepts_empty_map_and_rejects_excess_sources() {
        let empty_map = GpioIrqMap::new(64, &[], true).unwrap();
        assert!(!empty_map.has_source_for_line(0));
        assert_eq!(
            empty_map.source_for_line(0),
            Err(GpioError::InterruptsUnsupported)
        );
        let excess_sources = [0x3au32; 64];
        assert_eq!(
            GpioIrqMap::new(63, &excess_sources, true),
            Err(GpioError::InvalidIrqDescription)
        );
        assert_eq!(
            GpioIrqMap::new(64, &excess_sources, false),
            Err(GpioError::InterruptsUnsupported)
        );
    }

    #[test]
    fn gpio_line_leases_are_exclusive_and_reusable_after_release() {
        let mut allocator = GpioLineAllocator::new(64).unwrap();
        assert_eq!(allocator.acquire(9), Ok(()));
        assert_eq!(allocator.acquire(9), Err(GpioError::LineBusy));
        assert_eq!(allocator.release(9), Ok(()));
        assert_eq!(allocator.acquire(9), Ok(()));
        assert_eq!(allocator.release(10), Err(GpioError::LineNotAllocated));
        assert_eq!(allocator.acquire(64), Err(GpioError::LineOutOfRange));
    }

    #[test]
    fn gpio_layout_rejects_invalid_windows_and_input_bit_ranges() {
        let offsets = GpioOffsets {
            direction: 0,
            output: 0x10,
            input: 0x20,
            interrupt: 0x30,
        };
        assert_eq!(
            GpioLayout::new(GPIO_BASE + 1, 0x38, 64, offsets, 0),
            Err(GpioError::UnalignedWindow)
        );
        assert_eq!(
            GpioLayout::new(GPIO_BASE, 0x37, 64, offsets, 0),
            Err(GpioError::RegisterOutsideWindow)
        );
        assert_eq!(
            GpioLayout::new(GPIO_BASE, 0x38, 0, offsets, 0),
            Err(GpioError::InvalidLineCount)
        );
        assert_eq!(
            GpioLayout::new(GPIO_BASE, 0x38, 64, offsets, 1),
            Err(GpioError::InputBitOutOfRange)
        );
        assert_eq!(
            GpioLayout::new(usize::MAX - 7, 0x38, 64, offsets, 0),
            Err(GpioError::AddressOverflow)
        );
    }
}
