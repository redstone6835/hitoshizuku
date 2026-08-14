//! LS2X APB-DMA 硬件描述符编码。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DmaDirection {
    DeviceToMemory,
    MemoryToDevice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DmaTransfer {
    pub(crate) direction: DmaDirection,
    pub(crate) memory: u64,
    pub(crate) peripheral: u32,
    pub(crate) bytes: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ls2xDmaProtocolError {
    Unaligned,
    InvalidLength,
}

#[cfg(test)]
const NEXT_VALID: u32 = 1;
const COMMAND_INTERRUPT: u32 = 1 << 1;
const COMMAND_MEMORY_TO_DEVICE: u32 = 1 << 12;
#[cfg(test)]
const DESCRIPTOR_ALIGNMENT: u64 = 32;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ls2xDmaDescriptor {
    pub(crate) next_low: u32,
    pub(crate) memory_low: u32,
    pub(crate) peripheral: u32,
    pub(crate) words: u32,
    pub(crate) step_length: u32,
    pub(crate) step_count: u32,
    pub(crate) command: u32,
    pub(crate) status: u32,
    pub(crate) next_high: u32,
    pub(crate) memory_high: u32,
    pub(crate) reserved: [u32; 2],
}

impl Ls2xDmaDescriptor {
    pub(crate) fn single(transfer: DmaTransfer) -> Result<Self, Ls2xDmaProtocolError> {
        if transfer.bytes == 0 {
            return Err(Ls2xDmaProtocolError::InvalidLength);
        }
        if !transfer.memory.is_multiple_of(4)
            || !transfer.peripheral.is_multiple_of(4)
            || !transfer.bytes.is_multiple_of(4)
        {
            return Err(Ls2xDmaProtocolError::Unaligned);
        }
        let command = COMMAND_INTERRUPT
            | if transfer.direction == DmaDirection::MemoryToDevice {
                COMMAND_MEMORY_TO_DEVICE
            } else {
                0
            };
        Ok(Self {
            next_low: 0,
            memory_low: transfer.memory as u32,
            peripheral: transfer.peripheral,
            words: transfer.bytes / 4,
            step_length: 0,
            step_count: 1,
            command,
            status: 0,
            next_high: 0,
            memory_high: (transfer.memory >> 32) as u32,
            reserved: [0; 2],
        })
    }

    #[cfg(test)]
    pub(crate) fn link_next(&mut self, address: u64) -> Result<(), Ls2xDmaProtocolError> {
        if !address.is_multiple_of(DESCRIPTOR_ALIGNMENT) {
            return Err(Ls2xDmaProtocolError::Unaligned);
        }
        self.next_low = address as u32 | NEXT_VALID;
        self.next_high = (address >> 32) as u32;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn end_chain(&mut self) {
        self.next_low &= !NEXT_VALID;
        self.next_high = 0;
    }
}
