//! LS2K SDIO 命令、响应和寄存器编码的宿主侧测试。

// The harness imports complete production modules but exercises only their pure contracts.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../src/protocol.rs"]
mod protocol;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../src/registers.rs"]
mod registers;

#[cfg(test)]
mod tests {
    use super::protocol::{
        Command, DataDirection, MmcProtocolError, ResponseType, card_status_has_error,
        emmc_sector_count, r6_relative_address, sd_sector_count, transfer_argument,
    };
    use super::registers::{
        Ls2kSdioLayout, Ls2kSdioLayoutError, command_control, data_control, dma_data_control,
        prescaler,
    };

    fn set_response_bits(words: &mut [u32; 4], lsb: u8, width: u8, value: u32) {
        for index in 0..width {
            let position = lsb + index;
            let word = 3 - usize::from(position / 32);
            let bit = position % 32;
            let source = (value >> index) & 1;
            words[word] = (words[word] & !(1 << bit)) | (source << bit);
        }
    }

    #[test]
    fn command_control_encodes_response_and_data_flags() {
        assert_eq!(
            command_control(Command::new(0, 0, ResponseType::None, None)),
            (1 << 8) | (1 << 6)
        );
        assert_eq!(
            command_control(Command::new(2, 0, ResponseType::R2, None)),
            2 | (1 << 10) | (1 << 9) | (1 << 8) | (1 << 6)
        );
        assert_eq!(
            command_control(Command::new(
                18,
                7,
                ResponseType::R1,
                Some(DataDirection::Read),
            )),
            18 | (1 << 11) | (1 << 9) | (1 << 8) | (1 << 6)
        );
        assert!(!ResponseType::R2.requires_crc_check());
        assert!(!ResponseType::R3.requires_crc_check());
        assert!(ResponseType::R1.requires_crc_check());
        assert!(ResponseType::R1b.has_card_status());
        assert!(!ResponseType::R6.has_card_status());
        assert!(ResponseType::R7.requires_crc_check());
    }

    #[test]
    fn data_control_encodes_direction_width_and_block_count() {
        assert_eq!(
            data_control(DataDirection::Read, 4, 8).unwrap(),
            8 | (1 << 19) | (1 << 17) | (1 << 16) | (2 << 12)
        );
        assert_eq!(
            data_control(DataDirection::Write, 1, 1).unwrap(),
            1 | (1 << 20) | (3 << 12)
        );
        assert!(data_control(DataDirection::Read, 8, 4095).is_ok());
        assert_eq!(
            data_control(DataDirection::Read, 2, 1),
            Err(Ls2kSdioLayoutError::InvalidBusWidth)
        );
    }

    #[test]
    fn dma_data_control_uses_the_controller_dma_enable_bits() {
        assert_eq!(
            dma_data_control(4, 8).unwrap(),
            8 | (1 << 17) | (1 << 16) | (3 << 14)
        );
        assert_eq!(
            dma_data_control(2, 1),
            Err(Ls2kSdioLayoutError::InvalidBusWidth)
        );
    }

    #[test]
    fn sd_high_capacity_csd_reports_512_byte_sector_count() {
        let mut csd = [0u32; 4];
        set_response_bits(&mut csd, 126, 2, 1);
        set_response_bits(&mut csd, 48, 22, 0x1fff);
        assert_eq!(sd_sector_count(csd), Ok(8_388_608));
    }

    #[test]
    fn sd_legacy_csd_uses_read_length_and_multiplier() {
        let mut csd = [0u32; 4];
        set_response_bits(&mut csd, 126, 2, 0);
        set_response_bits(&mut csd, 80, 4, 9);
        set_response_bits(&mut csd, 62, 12, 4095);
        set_response_bits(&mut csd, 47, 3, 7);
        assert_eq!(sd_sector_count(csd), Ok(2_097_152));
    }

    #[test]
    fn emmc_extended_csd_sector_count_is_little_endian() {
        let mut ext_csd = [0u8; 512];
        ext_csd[212..216].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        assert_eq!(emmc_sector_count(&ext_csd), Ok(0x1234_5678));
        ext_csd[212..216].fill(0);
        assert_eq!(
            emmc_sector_count(&ext_csd),
            Err(MmcProtocolError::InvalidCapacity)
        );
    }

    #[test]
    fn response_helpers_validate_rca_status_and_addressing() {
        assert_eq!(r6_relative_address(0x1234_0000), Ok(0x1234));
        assert_eq!(
            r6_relative_address(1 << 15),
            Err(MmcProtocolError::CardStatus)
        );
        assert!(card_status_has_error(1 << 31));
        assert!(!card_status_has_error(1 << 8));
        assert_eq!(transfer_argument(7, true), Ok(7));
        assert_eq!(transfer_argument(7, false), Ok(3584));
        assert_eq!(
            transfer_argument(u64::from(u32::MAX) + 1, true),
            Err(MmcProtocolError::AddressOverflow)
        );
    }

    #[test]
    fn prescaler_uses_ceiling_division_without_exceeding_hardware_range() {
        assert_eq!(prescaler(125_000_000, 25_000_000), Some((5, 25_000_000)));
        assert_eq!(prescaler(125_000_000, 400_000), Some((255, 490_196)));
        assert_eq!(prescaler(125_000_000, 0), None);
    }

    #[test]
    fn register_layout_covers_fifo_interrupt_and_dma_windows() {
        let layout = Ls2kSdioLayout::new(0x1fe2_c000, 0x1000).unwrap();
        let registers = layout.registers();
        assert_eq!(registers.control, 0x1fe2_c000);
        assert_eq!(registers.command_status, 0x1fe2_c010);
        assert_eq!(registers.fifo, 0x1fe2_c040);
        assert_eq!(registers.interrupt_enable, 0x1fe2_c064);
        assert_eq!(layout.write_dma_order(), 0x1fe2_c400);
        assert_eq!(layout.read_dma_order(), 0x1fe2_c800);
        assert_eq!(
            Ls2kSdioLayout::new(0x1fe2_c002, 0x1000),
            Err(Ls2kSdioLayoutError::Unaligned)
        );
        assert_eq!(
            Ls2kSdioLayout::new(0x1fe2_c000, 0x800),
            Err(Ls2kSdioLayoutError::WindowTooSmall)
        );
    }
}
