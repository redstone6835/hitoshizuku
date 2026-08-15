const HOST_CAP_OFFSET: usize = 0x00;
const HOST_GHC_OFFSET: usize = 0x04;
const HOST_IS_OFFSET: usize = 0x08;
const HOST_PI_OFFSET: usize = 0x0c;
const HOST_VERSION_OFFSET: usize = 0x10;
const HOST_CAP2_OFFSET: usize = 0x24;
const HOST_BOHC_OFFSET: usize = 0x28;

const PORT_BASE_OFFSET: usize = 0x100;
const PORT_STRIDE: usize = 0x80;
const MIN_WINDOW_SIZE: usize = PORT_BASE_OFFSET + PORT_STRIDE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AhciRegisterLayoutError {
    Unaligned,
    WindowTooSmall,
    AddressOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AhciPortRegisters {
    pub(crate) command_list_base: usize,
    pub(crate) command_list_base_upper: usize,
    pub(crate) received_fis_base: usize,
    pub(crate) received_fis_base_upper: usize,
    pub(crate) interrupt_status: usize,
    pub(crate) interrupt_enable: usize,
    pub(crate) command: usize,
    pub(crate) task_file_data: usize,
    pub(crate) signature: usize,
    pub(crate) sata_status: usize,
    pub(crate) sata_control: usize,
    pub(crate) sata_error: usize,
    pub(crate) sata_active: usize,
    pub(crate) command_issue: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AhciRegisterLayout {
    base: usize,
    size: usize,
}

impl AhciRegisterLayout {
    pub(crate) fn new(base: usize, size: usize) -> Result<Self, AhciRegisterLayoutError> {
        if !base.is_multiple_of(4) {
            return Err(AhciRegisterLayoutError::Unaligned);
        }
        if size < MIN_WINDOW_SIZE {
            return Err(AhciRegisterLayoutError::WindowTooSmall);
        }
        base.checked_add(size)
            .ok_or(AhciRegisterLayoutError::AddressOverflow)?;
        Ok(Self { base, size })
    }

    pub(crate) const fn cap(self) -> usize {
        self.base + HOST_CAP_OFFSET
    }

    pub(crate) const fn ghc(self) -> usize {
        self.base + HOST_GHC_OFFSET
    }

    pub(crate) const fn interrupt_status(self) -> usize {
        self.base + HOST_IS_OFFSET
    }

    pub(crate) const fn ports_implemented(self) -> usize {
        self.base + HOST_PI_OFFSET
    }

    pub(crate) const fn version(self) -> usize {
        self.base + HOST_VERSION_OFFSET
    }

    pub(crate) const fn cap2(self) -> usize {
        self.base + HOST_CAP2_OFFSET
    }

    pub(crate) const fn bios_handoff(self) -> usize {
        self.base + HOST_BOHC_OFFSET
    }

    pub(crate) fn port(self, index: u32) -> Option<AhciPortRegisters> {
        let offset = PORT_BASE_OFFSET.checked_add((index as usize).checked_mul(PORT_STRIDE)?)?;
        if offset.checked_add(PORT_STRIDE)? > self.size {
            return None;
        }
        let base = self.base.checked_add(offset)?;
        Some(AhciPortRegisters {
            command_list_base: base,
            command_list_base_upper: base + 0x04,
            received_fis_base: base + 0x08,
            received_fis_base_upper: base + 0x0c,
            interrupt_status: base + 0x10,
            interrupt_enable: base + 0x14,
            command: base + 0x18,
            task_file_data: base + 0x20,
            signature: base + 0x24,
            sata_status: base + 0x28,
            sata_control: base + 0x2c,
            sata_error: base + 0x30,
            sata_active: base + 0x34,
            command_issue: base + 0x38,
        })
    }
}

pub(crate) const fn effective_port_map(
    capability: u32,
    hardware_map: u32,
    firmware_map: Option<u32>,
) -> Option<u32> {
    let port_count = (capability & 0x1f) + 1;
    let valid_mask = if port_count == 32 {
        u32::MAX
    } else {
        (1u32 << port_count) - 1
    };
    let mut selected = if hardware_map != 0 {
        hardware_map
    } else {
        match firmware_map {
            Some(map) => map,
            None => return None,
        }
    };
    if let Some(firmware_map) = firmware_map {
        selected &= firmware_map;
    }
    if selected == 0 || selected & !valid_mask != 0 {
        return None;
    }
    Some(selected)
}
