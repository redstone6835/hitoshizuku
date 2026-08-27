//! AHCI 命令编码与 ATA IDENTIFY 解析的宿主侧测试。

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
        AhciDmaLayout, AhciProtocolError, AtaCommand, IdentifyInfo, encode_command,
    };
    use super::registers::{AhciRegisterLayout, AhciRegisterLayoutError, effective_port_map};

    fn put_word(data: &mut [u8; 512], word: usize, value: u16) {
        data[word * 2..word * 2 + 2].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn read_dma_ext_encodes_all_48_lba_bits_and_sector_count() {
        let mut header = [0u8; 32];
        let mut table = [0u8; 144];
        encode_command(
            &mut header,
            &mut table,
            0x0012_3400,
            0x0020_0000,
            0x2000,
            AtaCommand::ReadDmaExt {
                lba: 0x1234_5678_9abc,
                sectors: 0x10,
            },
        )
        .unwrap();

        assert_eq!(&header[0..4], &[5, 0, 1, 0]);
        assert_eq!(&header[8..16], &[0x00, 0x34, 0x12, 0, 0, 0, 0, 0]);
        assert_eq!(
            &table[0..20],
            &[
                0x27, 0x80, 0x25, 0, 0xbc, 0x9a, 0x78, 0x40, 0x56, 0x34, 0x12, 0, 0x10, 0, 0, 0, 0,
                0, 0, 0,
            ]
        );
        assert_eq!(
            &table[128..144],
            &[
                0x00, 0x00, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0x1f, 0, 0x80
            ]
        );
    }

    #[test]
    fn write_and_flush_use_the_correct_header_direction_and_prdt_count() {
        let mut header = [0u8; 32];
        let mut table = [0u8; 144];
        encode_command(
            &mut header,
            &mut table,
            0x8000,
            0x9000,
            512,
            AtaCommand::WriteDmaExt { lba: 7, sectors: 1 },
        )
        .unwrap();
        assert_eq!(u16::from_le_bytes([header[0], header[1]]), 5 | (1 << 6));
        assert_eq!(table[2], 0x35);

        encode_command(
            &mut header,
            &mut table,
            0x8000,
            0,
            0,
            AtaCommand::FlushCacheExt,
        )
        .unwrap();
        assert_eq!(&header[0..4], &[5, 0, 0, 0]);
        assert_eq!(table[2], 0xea);
        assert!(table[128..144].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn command_encoder_rejects_invalid_alignment_length_and_lba() {
        let mut header = [0u8; 32];
        let mut table = [0u8; 144];
        assert_eq!(
            encode_command(
                &mut header,
                &mut table,
                0x8040,
                0x9000,
                512,
                AtaCommand::ReadDmaExt { lba: 0, sectors: 1 },
            ),
            Err(AhciProtocolError::Unaligned)
        );
        assert_eq!(
            encode_command(
                &mut header,
                &mut table,
                0x8000,
                0x9000,
                0x40_0001,
                AtaCommand::ReadDmaExt { lba: 0, sectors: 1 },
            ),
            Err(AhciProtocolError::DataTooLarge)
        );
        assert_eq!(
            encode_command(
                &mut header,
                &mut table,
                0x8000,
                0x9000,
                512,
                AtaCommand::ReadDmaExt {
                    lba: 1 << 48,
                    sectors: 1,
                },
            ),
            Err(AhciProtocolError::InvalidCommand)
        );
    }

    #[test]
    fn identify_parses_lba48_and_extended_logical_sector_size() {
        let mut data = [0u8; 512];
        put_word(&mut data, 49, (1 << 9) | (1 << 8));
        put_word(&mut data, 83, (1 << 10) | (1 << 13));
        let sectors = 0x0000_0001_2345_6789u64;
        for index in 0..4 {
            put_word(&mut data, 100 + index, (sectors >> (index * 16)) as u16);
        }
        put_word(&mut data, 106, (1 << 14) | (1 << 13) | (1 << 12) | 3);
        put_word(&mut data, 117, 2048);
        put_word(&mut data, 118, 0);
        put_word(&mut data, 217, 1);

        assert_eq!(
            IdentifyInfo::parse(&data),
            Ok(IdentifyInfo {
                sectors,
                logical_sector_size: 4096,
                physical_sector_size: 32768,
                supports_flush: true,
                rotational: false,
            })
        );
    }

    #[test]
    fn identify_rejects_missing_dma_lba48_and_zero_capacity() {
        let data = [0u8; 512];
        assert_eq!(
            IdentifyInfo::parse(&data),
            Err(AhciProtocolError::UnsupportedDevice)
        );
    }

    #[test]
    fn dma_layout_requires_ahci_alignment_and_32_bit_addresses() {
        assert!(AhciDmaLayout::new(0x1000, 0x2000, 0x3000, 0x4000, 0x10_0000).is_ok());
        assert_eq!(
            AhciDmaLayout::new(0x1001, 0x2000, 0x3000, 0x4000, 0x10_0000),
            Err(AhciProtocolError::Unaligned)
        );
        assert_eq!(
            AhciDmaLayout::new(0x1_0000_0000, 0x2000, 0x3000, 0x4000, 0x10_0000),
            Err(AhciProtocolError::AddressWidth)
        );
    }

    #[test]
    fn port_zero_registers_follow_the_standard_ahci_stride() {
        let layout = AhciRegisterLayout::new(0x400e_0000, 0x1_0000).unwrap();
        let port = layout.port(0).unwrap();

        assert_eq!(layout.cap(), 0x400e_0000);
        assert_eq!(layout.ghc(), 0x400e_0004);
        assert_eq!(layout.interrupt_status(), 0x400e_0008);
        assert_eq!(layout.ports_implemented(), 0x400e_000c);
        assert_eq!(port.command_list_base, 0x400e_0100);
        assert_eq!(port.received_fis_base, 0x400e_0108);
        assert_eq!(port.interrupt_status, 0x400e_0110);
        assert_eq!(port.command, 0x400e_0118);
        assert_eq!(port.task_file_data, 0x400e_0120);
        assert_eq!(port.sata_status, 0x400e_0128);
        assert_eq!(port.sata_error, 0x400e_0130);
        assert_eq!(port.command_issue, 0x400e_0138);
    }

    #[test]
    fn register_layout_rejects_short_unaligned_and_overflowing_windows() {
        assert_eq!(
            AhciRegisterLayout::new(0x400e_0001, 0x1_0000),
            Err(AhciRegisterLayoutError::Unaligned)
        );
        assert_eq!(
            AhciRegisterLayout::new(0x400e_0000, 0x17f),
            Err(AhciRegisterLayoutError::WindowTooSmall)
        );
        assert_eq!(
            AhciRegisterLayout::new(usize::MAX - 0xff, 0x1000),
            Err(AhciRegisterLayoutError::AddressOverflow)
        );
    }

    #[test]
    fn effective_port_map_is_bounded_by_capability_and_firmware() {
        let cap_two_ports = 1;
        assert_eq!(
            effective_port_map(cap_two_ports, 0b11, Some(0b01)),
            Some(0b01)
        );
        assert_eq!(effective_port_map(cap_two_ports, 0, Some(0b10)), Some(0b10));
        assert_eq!(effective_port_map(cap_two_ports, 0b100, None), None);
        assert_eq!(effective_port_map(cap_two_ports, 0, None), None);
    }
}
