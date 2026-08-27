//! SD 存储卡与 eMMC 初始化和块寻址所需的协议编码。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseType {
    None,
    R1,
    R1b,
    R2,
    R3,
    R6,
    R7,
}

impl ResponseType {
    pub(crate) const fn is_present(self) -> bool {
        !matches!(self, Self::None)
    }

    pub(crate) const fn is_long(self) -> bool {
        matches!(self, Self::R2)
    }

    pub(crate) const fn has_card_status(self) -> bool {
        matches!(self, Self::R1 | Self::R1b)
    }

    pub(crate) const fn requires_crc_check(self) -> bool {
        !matches!(self, Self::None | Self::R2 | Self::R3)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataDirection {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Command {
    pub(crate) index: u8,
    pub(crate) argument: u32,
    pub(crate) response: ResponseType,
    pub(crate) data: Option<DataDirection>,
}

impl Command {
    pub(crate) const fn new(
        index: u8,
        argument: u32,
        response: ResponseType,
        data: Option<DataDirection>,
    ) -> Self {
        Self {
            index,
            argument,
            response,
            data,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MmcProtocolError {
    UnsupportedCsd,
    InvalidCapacity,
    CardStatus,
    AddressOverflow,
}

const CARD_STATUS_ERROR_MASK: u32 = (1 << 31)
    | (1 << 30)
    | (1 << 29)
    | (1 << 28)
    | (1 << 27)
    | (1 << 26)
    | (1 << 24)
    | (1 << 23)
    | (1 << 22)
    | (1 << 21)
    | (1 << 20)
    | (1 << 19)
    | (1 << 16)
    | (1 << 15)
    | (1 << 13)
    | (1 << 7)
    | (1 << 3);
const R6_STATUS_ERROR_MASK: u32 = (1 << 15) | (1 << 14) | (1 << 13);

pub(crate) const fn card_status_has_error(status: u32) -> bool {
    status & CARD_STATUS_ERROR_MASK != 0
}

pub(crate) fn r6_relative_address(response: u32) -> Result<u16, MmcProtocolError> {
    if response & R6_STATUS_ERROR_MASK != 0 {
        return Err(MmcProtocolError::CardStatus);
    }
    let rca = (response >> 16) as u16;
    if rca == 0 {
        return Err(MmcProtocolError::CardStatus);
    }
    Ok(rca)
}

pub(crate) fn transfer_argument(lba: u64, high_capacity: bool) -> Result<u32, MmcProtocolError> {
    let address = if high_capacity {
        lba
    } else {
        lba.checked_mul(512)
            .ok_or(MmcProtocolError::AddressOverflow)?
    };
    u32::try_from(address).map_err(|_| MmcProtocolError::AddressOverflow)
}

pub(crate) fn sd_sector_count(csd: [u32; 4]) -> Result<u64, MmcProtocolError> {
    match response_bits(csd, 126, 2) {
        Some(1) => response_bits(csd, 48, 22)
            .and_then(|size| u64::from(size).checked_add(1))
            .and_then(|size| size.checked_mul(1024))
            .filter(|sectors| *sectors != 0)
            .ok_or(MmcProtocolError::InvalidCapacity),
        Some(0) => {
            let read_block_length =
                response_bits(csd, 80, 4).ok_or(MmcProtocolError::InvalidCapacity)?;
            let size = response_bits(csd, 62, 12).ok_or(MmcProtocolError::InvalidCapacity)?;
            let multiplier = response_bits(csd, 47, 3).ok_or(MmcProtocolError::InvalidCapacity)?;
            if read_block_length > 31 || multiplier > 7 {
                return Err(MmcProtocolError::InvalidCapacity);
            }
            let bytes = u64::from(size)
                .checked_add(1)
                .and_then(|blocks| blocks.checked_shl(multiplier + 2))
                .and_then(|blocks| blocks.checked_shl(read_block_length))
                .ok_or(MmcProtocolError::InvalidCapacity)?;
            let sectors = bytes / 512;
            (sectors != 0)
                .then_some(sectors)
                .ok_or(MmcProtocolError::InvalidCapacity)
        }
        _ => Err(MmcProtocolError::UnsupportedCsd),
    }
}

pub(crate) fn emmc_sector_count(ext_csd: &[u8]) -> Result<u64, MmcProtocolError> {
    let bytes: [u8; 4] = ext_csd
        .get(212..216)
        .and_then(|value| value.try_into().ok())
        .ok_or(MmcProtocolError::InvalidCapacity)?;
    let sectors = u32::from_le_bytes(bytes);
    (sectors != 0)
        .then_some(u64::from(sectors))
        .ok_or(MmcProtocolError::InvalidCapacity)
}

fn response_bits(words: [u32; 4], lsb: u8, width: u8) -> Option<u32> {
    if width == 0 || width > 32 || u16::from(lsb) + u16::from(width) > 128 {
        return None;
    }
    let mut value = 0u32;
    for index in 0..width {
        let position = lsb + index;
        let word = 3usize.checked_sub(usize::from(position / 32))?;
        let bit = position % 32;
        value |= ((words[word] >> bit) & 1) << index;
    }
    Some(value)
}
