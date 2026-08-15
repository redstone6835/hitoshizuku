const COMMAND_HEADER_SIZE: usize = 32;
const COMMAND_TABLE_SIZE: usize = 144;
const COMMAND_FIS_SIZE: usize = 20;
const PRDT_OFFSET: usize = 128;
const PRDT_ENTRY_SIZE: usize = 16;
const MAX_PRDT_BYTES: usize = 4 * 1024 * 1024;
const MAX_LBA48: u64 = (1u64 << 48) - 1;

const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_FLUSH_CACHE_EXT: u8 = 0xea;
const ATA_CMD_IDENTIFY_DEVICE: u8 = 0xec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AhciProtocolError {
    InvalidBuffer,
    Unaligned,
    AddressWidth,
    DataTooLarge,
    InvalidCommand,
    UnsupportedDevice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AtaCommand {
    IdentifyDevice,
    ReadDmaExt { lba: u64, sectors: u16 },
    WriteDmaExt { lba: u64, sectors: u16 },
    FlushCacheExt,
}

impl AtaCommand {
    const fn opcode(self) -> u8 {
        match self {
            Self::IdentifyDevice => ATA_CMD_IDENTIFY_DEVICE,
            Self::ReadDmaExt { .. } => ATA_CMD_READ_DMA_EXT,
            Self::WriteDmaExt { .. } => ATA_CMD_WRITE_DMA_EXT,
            Self::FlushCacheExt => ATA_CMD_FLUSH_CACHE_EXT,
        }
    }

    const fn writes_to_device(self) -> bool {
        matches!(self, Self::WriteDmaExt { .. })
    }

    const fn needs_data(self) -> bool {
        !matches!(self, Self::FlushCacheExt)
    }

    fn validate(self, data_len: usize) -> Result<(), AhciProtocolError> {
        match self {
            Self::IdentifyDevice if data_len == 512 => Ok(()),
            Self::IdentifyDevice => Err(AhciProtocolError::InvalidCommand),
            Self::ReadDmaExt { lba, sectors } | Self::WriteDmaExt { lba, sectors }
                if lba <= MAX_LBA48 && sectors != 0 && data_len != 0 =>
            {
                Ok(())
            }
            Self::ReadDmaExt { .. } | Self::WriteDmaExt { .. } => {
                Err(AhciProtocolError::InvalidCommand)
            }
            Self::FlushCacheExt if data_len == 0 => Ok(()),
            Self::FlushCacheExt => Err(AhciProtocolError::InvalidCommand),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AhciDmaLayout {
    pub(crate) command_list: u32,
    pub(crate) received_fis: u32,
    pub(crate) command_table: u32,
    pub(crate) data: u32,
    pub(crate) data_len: usize,
}

impl AhciDmaLayout {
    pub(crate) fn new(
        command_list: usize,
        received_fis: usize,
        command_table: usize,
        data: usize,
        data_len: usize,
    ) -> Result<Self, AhciProtocolError> {
        if !command_list.is_multiple_of(1024)
            || !received_fis.is_multiple_of(256)
            || !command_table.is_multiple_of(128)
            || !data.is_multiple_of(2)
        {
            return Err(AhciProtocolError::Unaligned);
        }
        if data_len == 0 || data_len > MAX_PRDT_BYTES {
            return Err(AhciProtocolError::DataTooLarge);
        }
        require_32bit_range(command_list, 1024)?;
        require_32bit_range(received_fis, 256)?;
        require_32bit_range(command_table, COMMAND_TABLE_SIZE)?;
        require_32bit_range(data, data_len)?;
        Ok(Self {
            command_list: command_list as u32,
            received_fis: received_fis as u32,
            command_table: command_table as u32,
            data: data as u32,
            data_len,
        })
    }
}

fn require_32bit_range(start: usize, len: usize) -> Result<(), AhciProtocolError> {
    let last = start
        .checked_add(len.saturating_sub(1))
        .ok_or(AhciProtocolError::AddressWidth)?;
    if u32::try_from(start).is_err() || u32::try_from(last).is_err() {
        return Err(AhciProtocolError::AddressWidth);
    }
    Ok(())
}

pub(crate) fn encode_command(
    header: &mut [u8],
    table: &mut [u8],
    command_table_dma: usize,
    data_dma: usize,
    data_len: usize,
    command: AtaCommand,
) -> Result<(), AhciProtocolError> {
    if header.len() < COMMAND_HEADER_SIZE || table.len() < COMMAND_TABLE_SIZE {
        return Err(AhciProtocolError::InvalidBuffer);
    }
    if !command_table_dma.is_multiple_of(128) {
        return Err(AhciProtocolError::Unaligned);
    }
    require_32bit_range(command_table_dma, COMMAND_TABLE_SIZE)?;
    command.validate(data_len)?;
    if command.needs_data() {
        if !data_dma.is_multiple_of(2) {
            return Err(AhciProtocolError::Unaligned);
        }
        if data_len > MAX_PRDT_BYTES {
            return Err(AhciProtocolError::DataTooLarge);
        }
        require_32bit_range(data_dma, data_len)?;
    }

    header[..COMMAND_HEADER_SIZE].fill(0);
    table[..COMMAND_TABLE_SIZE].fill(0);

    let flags = 5u16
        | if command.writes_to_device() {
            1 << 6
        } else {
            0
        };
    header[0..2].copy_from_slice(&flags.to_le_bytes());
    let prdt_length = u16::from(command.needs_data());
    header[2..4].copy_from_slice(&prdt_length.to_le_bytes());
    header[8..12].copy_from_slice(&(command_table_dma as u32).to_le_bytes());

    encode_register_fis(&mut table[..COMMAND_FIS_SIZE], command);
    if command.needs_data() {
        table[PRDT_OFFSET..PRDT_OFFSET + 4].copy_from_slice(&(data_dma as u32).to_le_bytes());
        let dbc =
            u32::try_from(data_len - 1).map_err(|_| AhciProtocolError::DataTooLarge)? | (1 << 31);
        table[PRDT_OFFSET + 12..PRDT_OFFSET + PRDT_ENTRY_SIZE].copy_from_slice(&dbc.to_le_bytes());
    }
    Ok(())
}

fn encode_register_fis(fis: &mut [u8], command: AtaCommand) {
    fis.fill(0);
    fis[0] = 0x27;
    fis[1] = 1 << 7;
    fis[2] = command.opcode();
    match command {
        AtaCommand::ReadDmaExt { lba, sectors } | AtaCommand::WriteDmaExt { lba, sectors } => {
            fis[4] = lba as u8;
            fis[5] = (lba >> 8) as u8;
            fis[6] = (lba >> 16) as u8;
            fis[7] = 1 << 6;
            fis[8] = (lba >> 24) as u8;
            fis[9] = (lba >> 32) as u8;
            fis[10] = (lba >> 40) as u8;
            fis[12] = sectors as u8;
            fis[13] = (sectors >> 8) as u8;
        }
        AtaCommand::IdentifyDevice | AtaCommand::FlushCacheExt => {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IdentifyInfo {
    pub(crate) sectors: u64,
    pub(crate) logical_sector_size: u32,
    pub(crate) physical_sector_size: u32,
    pub(crate) supports_flush: bool,
    pub(crate) rotational: bool,
}

impl IdentifyInfo {
    pub(crate) fn parse(data: &[u8]) -> Result<Self, AhciProtocolError> {
        if data.len() < 512 {
            return Err(AhciProtocolError::InvalidBuffer);
        }
        let capabilities = word(data, 49)?;
        let command_sets = word(data, 83)?;
        if capabilities & ((1 << 8) | (1 << 9)) != (1 << 8) | (1 << 9)
            || command_sets & (1 << 10) == 0
        {
            return Err(AhciProtocolError::UnsupportedDevice);
        }

        let mut sectors = 0u64;
        for index in 0..4 {
            sectors |= u64::from(word(data, 100 + index)?) << (index * 16);
        }
        if sectors == 0 {
            return Err(AhciProtocolError::UnsupportedDevice);
        }

        let sector_info = word(data, 106)?;
        let logical_sector_size = if sector_info & 0xc000 == 0x4000 && sector_info & (1 << 12) != 0
        {
            let words = u32::from(word(data, 117)?) | (u32::from(word(data, 118)?) << 16);
            words
                .checked_mul(2)
                .filter(|bytes| *bytes >= 512 && bytes.is_power_of_two())
                .ok_or(AhciProtocolError::UnsupportedDevice)?
        } else {
            512
        };
        let physical_sector_size = if sector_info & 0xc000 == 0x4000 && sector_info & (1 << 13) != 0
        {
            logical_sector_size
                .checked_shl(u32::from(sector_info & 0x0f))
                .ok_or(AhciProtocolError::UnsupportedDevice)?
        } else {
            logical_sector_size
        };
        let rotation_rate = word(data, 217)?;

        Ok(Self {
            sectors,
            logical_sector_size,
            physical_sector_size,
            supports_flush: command_sets & (1 << 13) != 0,
            rotational: rotation_rate != 1,
        })
    }
}

fn word(data: &[u8], index: usize) -> Result<u16, AhciProtocolError> {
    let offset = index
        .checked_mul(2)
        .ok_or(AhciProtocolError::InvalidBuffer)?;
    let bytes = data
        .get(offset..offset + 2)
        .ok_or(AhciProtocolError::InvalidBuffer)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}
