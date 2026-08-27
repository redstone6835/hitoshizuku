//! LS2X APB-DMA 描述符和寄存器编码的宿主侧测试。

// The harness imports complete production modules but exercises only their pure contracts.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../src/layout.rs"]
mod layout;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../src/protocol.rs"]
mod protocol;

#[cfg(test)]
mod tests {
    use core::mem::{align_of, size_of};

    use super::layout::{
        Ls2xDmaMmioLayout, Ls2xDmaMmioLayoutError, apply_selector, start_order, stop_order,
    };
    use super::protocol::{
        DmaDirection, DmaTransfer, Ls2xDmaDescriptor, Ls2xDmaProtocolError,
    };

    #[test]
    fn descriptor_layout_matches_the_48_byte_hardware_contract() {
        assert_eq!(size_of::<Ls2xDmaDescriptor>(), 48);
        assert_eq!(align_of::<Ls2xDmaDescriptor>(), 4);
    }

    #[test]
    fn single_descriptor_encodes_both_address_halves_and_direction() {
        let read = Ls2xDmaDescriptor::single(DmaTransfer {
            direction: DmaDirection::DeviceToMemory,
            memory: 0x1234_5678_9abc_def0,
            peripheral: 0x1fe2_c040,
            bytes: 4096,
        })
        .unwrap();
        assert_eq!(read.next_low, 0);
        assert_eq!(read.next_high, 0);
        assert_eq!(read.memory_low, 0x9abc_def0);
        assert_eq!(read.memory_high, 0x1234_5678);
        assert_eq!(read.peripheral, 0x1fe2_c040);
        assert_eq!(read.words, 1024);
        assert_eq!(read.step_length, 0);
        assert_eq!(read.step_count, 1);
        assert_eq!(read.command, 1 << 1);

        let write = Ls2xDmaDescriptor::single(DmaTransfer {
            direction: DmaDirection::MemoryToDevice,
            memory: 0x2000,
            peripheral: 0x1fe2_c040,
            bytes: 512,
        })
        .unwrap();
        assert_eq!(write.command, (1 << 12) | (1 << 1));
    }

    #[test]
    fn descriptor_rejects_invalid_alignment_and_length() {
        let transfer = |memory, peripheral, bytes| DmaTransfer {
            direction: DmaDirection::DeviceToMemory,
            memory,
            peripheral,
            bytes,
        };
        assert_eq!(
            Ls2xDmaDescriptor::single(transfer(0x2001, 0x1fe2_c040, 512)),
            Err(Ls2xDmaProtocolError::Unaligned)
        );
        assert_eq!(
            Ls2xDmaDescriptor::single(transfer(0x2000, 0x1fe2_c042, 512)),
            Err(Ls2xDmaProtocolError::Unaligned)
        );
        assert_eq!(
            Ls2xDmaDescriptor::single(transfer(0x2000, 0x1fe2_c040, 510)),
            Err(Ls2xDmaProtocolError::Unaligned)
        );
        assert_eq!(
            Ls2xDmaDescriptor::single(transfer(0x2000, 0x1fe2_c040, 0)),
            Err(Ls2xDmaProtocolError::InvalidLength)
        );
    }

    #[test]
    fn descriptor_links_require_the_order_register_alignment() {
        let mut descriptor = Ls2xDmaDescriptor::single(DmaTransfer {
            direction: DmaDirection::DeviceToMemory,
            memory: 0x2000,
            peripheral: 0x1fe2_c040,
            bytes: 512,
        })
        .unwrap();
        descriptor.link_next(0x1_2345_6000).unwrap();
        assert_eq!(descriptor.next_low, 0x2345_6001);
        assert_eq!(descriptor.next_high, 1);
        assert_eq!(
            descriptor.link_next(0x1_2345_6004),
            Err(Ls2xDmaProtocolError::Unaligned)
        );
        descriptor.end_chain();
        assert_eq!(descriptor.next_low & 1, 0);
    }

    #[test]
    fn order_register_preserves_the_descriptor_address() {
        assert_eq!(start_order(0x1_2345_6000, true).unwrap(), 0x1_2345_6009);
        assert_eq!(start_order(0x2345_6000, false).unwrap(), 0x2345_6008);
        assert_eq!(
            start_order(0x2345_6004, true),
            Err(Ls2xDmaMmioLayoutError::Unaligned)
        );
        assert_eq!(stop_order(0x1_2345_6009, true), 0x1_2345_6011);
    }

    #[test]
    fn selector_updates_only_the_requested_bit() {
        assert_eq!(apply_selector(0b1010, 2, true), Some(0b1110));
        assert_eq!(apply_selector(0b1010, 1, false), Some(0b1000));
        assert_eq!(apply_selector(0, 64, true), None);
    }

    #[test]
    fn mmio_layout_requires_one_aligned_64_bit_order_register() {
        assert_eq!(Ls2xDmaMmioLayout::new(0x1fe0_0c10, 8).unwrap().order(), 0x1fe0_0c10);
        assert_eq!(
            Ls2xDmaMmioLayout::new(0x1fe0_0c14, 8),
            Err(Ls2xDmaMmioLayoutError::Unaligned)
        );
        assert_eq!(
            Ls2xDmaMmioLayout::new(0x1fe0_0c10, 4),
            Err(Ls2xDmaMmioLayoutError::WindowTooSmall)
        );
    }
}
