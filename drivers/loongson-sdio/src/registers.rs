//! LS2K SDIO 控制器的寄存器布局和控制字编码。

use crate::protocol::{Command, DataDirection};

const COMMAND_WITH_DATA: u32 = 1 << 11;
const COMMAND_LONG_RESPONSE: u32 = 1 << 10;
const COMMAND_WAIT_RESPONSE: u32 = 1 << 9;
const COMMAND_START: u32 = 1 << 8;
const COMMAND_SENDER_HOST: u32 = 1 << 6;

const DATA_8_BIT_BUS: u32 = 1 << 26;
const DATA_WRITE_AFTER_RESPONSE: u32 = 1 << 20;
const DATA_READ_AFTER_COMMAND: u32 = 1 << 19;
const DATA_BLOCK_MODE: u32 = 1 << 17;
const DATA_4_BIT_BUS: u32 = 1 << 16;
const DATA_READ_START: u32 = 2 << 12;
const DATA_WRITE_START: u32 = 3 << 12;
const DATA_BLOCK_COUNT_MASK: u32 = 0xfff;
const DATA_DMA_ENABLE: u32 = 3 << 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ls2kSdioLayoutError {
    Unaligned,
    WindowTooSmall,
    AddressOverflow,
    InvalidBusWidth,
    InvalidBlockCount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ls2kSdioRegisters {
    pub(crate) control: usize,
    pub(crate) prescaler: usize,
    pub(crate) command_argument: usize,
    pub(crate) command_control: usize,
    pub(crate) command_status: usize,
    pub(crate) response: [usize; 4],
    pub(crate) data_timer: usize,
    pub(crate) block_size: usize,
    pub(crate) data_control: usize,
    pub(crate) data_count: usize,
    pub(crate) data_status: usize,
    pub(crate) fifo_status: usize,
    pub(crate) interrupt_status: usize,
    pub(crate) fifo: usize,
    pub(crate) interrupt_enable: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ls2kSdioLayout {
    base: usize,
}

impl Ls2kSdioLayout {
    pub(crate) fn new(base: usize, size: usize) -> Result<Self, Ls2kSdioLayoutError> {
        if !base.is_multiple_of(4) {
            return Err(Ls2kSdioLayoutError::Unaligned);
        }
        if size < 0x808 {
            return Err(Ls2kSdioLayoutError::WindowTooSmall);
        }
        base.checked_add(0x807)
            .ok_or(Ls2kSdioLayoutError::AddressOverflow)?;
        Ok(Self { base })
    }

    pub(crate) fn registers(self) -> Ls2kSdioRegisters {
        Ls2kSdioRegisters {
            control: self.base,
            prescaler: self.base + 0x04,
            command_argument: self.base + 0x08,
            command_control: self.base + 0x0c,
            command_status: self.base + 0x10,
            response: [
                self.base + 0x14,
                self.base + 0x18,
                self.base + 0x1c,
                self.base + 0x20,
            ],
            data_timer: self.base + 0x24,
            block_size: self.base + 0x28,
            data_control: self.base + 0x2c,
            data_count: self.base + 0x30,
            data_status: self.base + 0x34,
            fifo_status: self.base + 0x38,
            interrupt_status: self.base + 0x3c,
            fifo: self.base + 0x40,
            interrupt_enable: self.base + 0x64,
        }
    }

    #[cfg(test)]
    pub(crate) const fn write_dma_order(self) -> usize {
        self.base + 0x400
    }

    #[cfg(test)]
    pub(crate) const fn read_dma_order(self) -> usize {
        self.base + 0x800
    }
}

pub(crate) const fn command_control(command: Command) -> u32 {
    (command.index & 0x3f) as u32
        | COMMAND_START
        | COMMAND_SENDER_HOST
        | if command.response.is_present() {
            COMMAND_WAIT_RESPONSE
        } else {
            0
        }
        | if command.response.is_long() {
            COMMAND_LONG_RESPONSE
        } else {
            0
        }
        | if command.data.is_some() {
            COMMAND_WITH_DATA
        } else {
            0
        }
}

pub(crate) fn data_control(
    direction: DataDirection,
    bus_width: u8,
    blocks: u32,
) -> Result<u32, Ls2kSdioLayoutError> {
    if blocks == 0 || blocks > DATA_BLOCK_COUNT_MASK {
        return Err(Ls2kSdioLayoutError::InvalidBlockCount);
    }
    let width = bus_width_control(bus_width)?;
    let direction = match direction {
        DataDirection::Read => DATA_READ_AFTER_COMMAND | DATA_READ_START,
        DataDirection::Write => DATA_WRITE_AFTER_RESPONSE | DATA_WRITE_START,
    };
    Ok(blocks | width | direction | if blocks > 1 { DATA_BLOCK_MODE } else { 0 })
}

pub(crate) fn dma_data_control(bus_width: u8, blocks: u32) -> Result<u32, Ls2kSdioLayoutError> {
    if blocks == 0 || blocks > DATA_BLOCK_COUNT_MASK {
        return Err(Ls2kSdioLayoutError::InvalidBlockCount);
    }
    Ok(blocks
        | bus_width_control(bus_width)?
        | DATA_DMA_ENABLE
        | if blocks > 1 { DATA_BLOCK_MODE } else { 0 })
}

fn bus_width_control(bus_width: u8) -> Result<u32, Ls2kSdioLayoutError> {
    match bus_width {
        1 => Ok(0),
        4 => Ok(DATA_4_BIT_BUS),
        8 => Ok(DATA_8_BIT_BUS),
        _ => Err(Ls2kSdioLayoutError::InvalidBusWidth),
    }
}

pub(crate) fn prescaler(input_hz: u64, target_hz: u64) -> Option<(u32, u64)> {
    if input_hz == 0 || target_hz == 0 {
        return None;
    }
    let divisor = input_hz
        .checked_add(target_hz - 1)?
        .checked_div(target_hz)?
        .clamp(1, 255);
    Some((divisor as u32, input_hz / divisor))
}
