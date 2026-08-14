use alloc::{vec, vec::Vec};

const REGISTER_WIDTH: usize = core::mem::size_of::<u32>();
const REQUIRED_WINDOW_SIZE: usize = 0x18;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinctrlError {
    EmptyState,
    UnknownGroup,
    UnknownFunction,
    OutOfMemory,
    InvalidCombination,
    ConflictingUpdates,
    UnalignedWindow,
    WindowTooSmall,
    AddressOverflow,
    UpdateOutsideWindow,
    InvalidUpdate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinGroup {
    SataLed,
    Gmac1,
    Dvo0LioUart,
    Uart1,
    Uart2,
    Dvo1Camera,
    Can0,
    Can1,
    HdaI2s,
    I2c0,
    I2c1,
    Uart0,
    Nand,
    Pwm0,
    Pwm1,
    Pwm2,
    Pwm3,
    Sdio,
}

impl PinGroup {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "sata_led" => Self::SataLed,
            "gmac1" => Self::Gmac1,
            "dvo0_lio_uart" => Self::Dvo0LioUart,
            "uart1" => Self::Uart1,
            "uart2" => Self::Uart2,
            "dvo1_camera" => Self::Dvo1Camera,
            "can0" => Self::Can0,
            "can1" => Self::Can1,
            "hda_i2s" => Self::HdaI2s,
            "i2c0" => Self::I2c0,
            "i2c1" => Self::I2c1,
            "uart0" => Self::Uart0,
            "nand" => Self::Nand,
            "pwm0" => Self::Pwm0,
            "pwm1" => Self::Pwm1,
            "pwm2" => Self::Pwm2,
            "pwm3" => Self::Pwm3,
            "sdio" => Self::Sdio,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinFunction {
    SataLed,
    Gpio,
    Gmac1,
    Dvo0,
    Lio,
    Uart1Mode4,
    Uart1Mode2,
    Uart1Mode1,
    Uart2Mode4,
    Uart2Mode2,
    Uart2Mode1,
    Dvo1,
    Camera,
    Can0,
    Can1,
    Hda,
    I2s,
    I2c0,
    I2c1,
    Uart0Mode4,
    Uart0Mode2,
    Uart0Mode1,
    Nand,
    Pwm0,
    Pwm1,
    Pwm2,
    Pwm3,
    Sdio,
}

impl PinFunction {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "sata_led" => Self::SataLed,
            "gpio" => Self::Gpio,
            "gmac1" => Self::Gmac1,
            "dvo0" => Self::Dvo0,
            "lio" => Self::Lio,
            "uart1_4" => Self::Uart1Mode4,
            "uart1_2" => Self::Uart1Mode2,
            "uart1_1" => Self::Uart1Mode1,
            "uart2_4" => Self::Uart2Mode4,
            "uart2_2" => Self::Uart2Mode2,
            "uart2_1" => Self::Uart2Mode1,
            "dvo1" => Self::Dvo1,
            "camera" => Self::Camera,
            "can0" => Self::Can0,
            "can1" => Self::Can1,
            "hda" => Self::Hda,
            "i2s" => Self::I2s,
            "i2c0" => Self::I2c0,
            "i2c1" => Self::I2c1,
            "uart0_4" => Self::Uart0Mode4,
            "uart0_2" => Self::Uart0Mode2,
            "uart0_1" => Self::Uart0Mode1,
            "nand" => Self::Nand,
            "pwm0" => Self::Pwm0,
            "pwm1" => Self::Pwm1,
            "pwm2" => Self::Pwm2,
            "pwm3" => Self::Pwm3,
            "sdio" => Self::Sdio,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MuxUpdate {
    pub(crate) offset: usize,
    pub(crate) mask: u32,
    pub(crate) value: u32,
}

impl MuxUpdate {
    const fn bit(offset: usize, bit: u32, value: bool) -> Self {
        let mask = 1u32 << bit;
        Self {
            offset,
            mask,
            value: if value { mask } else { 0 },
        }
    }

    const fn field(offset: usize, shift: u32, width: u32, value: u32) -> Self {
        let mask = ((1u32 << width) - 1) << shift;
        Self {
            offset,
            mask,
            value: value << shift,
        }
    }

    pub(crate) const fn apply(self, current: u32) -> u32 {
        (current & !self.mask) | self.value
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PinctrlMmioLayout {
    base: usize,
    size: usize,
}

impl PinctrlMmioLayout {
    pub(crate) fn new(base: usize, size: usize) -> Result<Self, PinctrlError> {
        if !base.is_multiple_of(REGISTER_WIDTH) {
            return Err(PinctrlError::UnalignedWindow);
        }
        if size < REQUIRED_WINDOW_SIZE {
            return Err(PinctrlError::WindowTooSmall);
        }
        base.checked_add(size)
            .ok_or(PinctrlError::AddressOverflow)?;
        Ok(Self { base, size })
    }

    pub(crate) fn address(self, update: MuxUpdate) -> Result<usize, PinctrlError> {
        if !update.offset.is_multiple_of(REGISTER_WIDTH)
            || update.mask == 0
            || update.value & !update.mask != 0
        {
            return Err(PinctrlError::InvalidUpdate);
        }
        let end = update
            .offset
            .checked_add(REGISTER_WIDTH)
            .ok_or(PinctrlError::AddressOverflow)?;
        if end > self.size {
            return Err(PinctrlError::UpdateOutsideWindow);
        }
        self.base
            .checked_add(update.offset)
            .ok_or(PinctrlError::AddressOverflow)
    }
}

pub(crate) fn decode_state(
    group: PinGroup,
    function: PinFunction,
) -> Result<Vec<MuxUpdate>, PinctrlError> {
    use PinFunction as F;
    use PinGroup as G;

    let updates = match (group, function) {
        (G::SataLed, F::SataLed) => vec![MuxUpdate::bit(0, 8, true)],
        (G::Gmac1, F::Gmac1) => vec![MuxUpdate::bit(0, 3, true)],
        (G::Can0, F::Can0) => vec![MuxUpdate::bit(0, 16, true)],
        (G::Can1, F::Can1) => vec![MuxUpdate::bit(0, 17, true)],
        (G::I2c0, F::I2c0) => vec![MuxUpdate::bit(0, 10, true)],
        (G::I2c1, F::I2c1) => vec![MuxUpdate::bit(0, 11, true)],
        (G::Nand, F::Nand) => vec![MuxUpdate::bit(0, 9, true)],
        (G::Pwm0, F::Pwm0) => vec![MuxUpdate::bit(0, 12, true)],
        (G::Pwm1, F::Pwm1) => vec![MuxUpdate::bit(0, 13, true)],
        (G::Pwm2, F::Pwm2) => vec![MuxUpdate::bit(0, 14, true)],
        (G::Pwm3, F::Pwm3) => vec![MuxUpdate::bit(0, 15, true)],
        (G::Sdio, F::Sdio) => vec![MuxUpdate::bit(0, 20, true)],
        (G::Dvo0LioUart, F::Dvo0) => dvo_lio_updates(true),
        (G::Dvo0LioUart, F::Lio) => dvo_lio_updates(false),
        (G::Uart1, F::Uart1Mode4) => uart_updates(true, 0xf),
        (G::Uart1, F::Uart1Mode2) => uart_updates(true, 0x3),
        (G::Uart1, F::Uart1Mode1) => uart_updates(true, 0x1),
        (G::Uart2, F::Uart2Mode4) => uart_updates(false, 0xf),
        (G::Uart2, F::Uart2Mode2) => uart_updates(false, 0x3),
        (G::Uart2, F::Uart2Mode1) => uart_updates(false, 0x1),
        (G::Dvo1Camera, F::Dvo1) => vec![
            MuxUpdate::bit(0x10, 4, true),
            MuxUpdate::bit(0x10, 5, false),
        ],
        (G::Dvo1Camera, F::Camera) => vec![
            MuxUpdate::bit(0x10, 4, false),
            MuxUpdate::bit(0x10, 5, true),
        ],
        (G::HdaI2s, F::Hda) => vec![MuxUpdate::bit(0, 6, false), MuxUpdate::bit(0, 4, true)],
        (G::HdaI2s, F::I2s) => vec![MuxUpdate::bit(0, 6, true), MuxUpdate::bit(0, 4, false)],
        (G::Uart0, F::Uart0Mode4) => vec![MuxUpdate::field(0x08, 0, 4, 0xf)],
        (G::Uart0, F::Uart0Mode2) => vec![MuxUpdate::field(0x08, 0, 4, 0x3)],
        (G::Uart0, F::Uart0Mode1) => vec![MuxUpdate::field(0x08, 0, 4, 0x1)],
        (group, F::Gpio) if gpio_muxable(group) => gpio_updates(group),
        _ => return Err(PinctrlError::InvalidCombination),
    };
    Ok(updates)
}

pub(crate) fn merge_states(
    states: &[(PinGroup, PinFunction)],
) -> Result<Vec<MuxUpdate>, PinctrlError> {
    let mut merged: Vec<MuxUpdate> = Vec::new();
    for &(group, function) in states {
        merge_updates(&mut merged, &decode_state(group, function)?)?;
    }
    Ok(merged)
}

pub(crate) fn merge_updates(
    merged: &mut Vec<MuxUpdate>,
    updates: &[MuxUpdate],
) -> Result<(), PinctrlError> {
    merged
        .try_reserve(updates.len())
        .map_err(|_| PinctrlError::OutOfMemory)?;
    for &update in updates {
        if let Some(current) = merged
            .iter_mut()
            .find(|current| current.offset == update.offset)
        {
            let overlap = current.mask & update.mask;
            if (current.value ^ update.value) & overlap != 0 {
                return Err(PinctrlError::ConflictingUpdates);
            }
            current.value = (current.value & !update.mask) | update.value;
            current.mask |= update.mask;
        } else {
            merged.push(update);
        }
    }
    Ok(())
}

pub(crate) fn decode_named_state(
    groups: &[&str],
    function: &str,
) -> Result<Vec<MuxUpdate>, PinctrlError> {
    if groups.is_empty() {
        return Err(PinctrlError::EmptyState);
    }
    let function = PinFunction::parse(function).ok_or(PinctrlError::UnknownFunction)?;
    let mut states = Vec::new();
    states
        .try_reserve(groups.len())
        .map_err(|_| PinctrlError::OutOfMemory)?;
    for name in groups {
        let group = PinGroup::parse(name).ok_or(PinctrlError::UnknownGroup)?;
        states.push((group, function));
    }
    merge_states(&states)
}

const fn gpio_muxable(group: PinGroup) -> bool {
    matches!(
        group,
        PinGroup::SataLed
            | PinGroup::Can0
            | PinGroup::Can1
            | PinGroup::HdaI2s
            | PinGroup::I2c0
            | PinGroup::I2c1
            | PinGroup::Nand
            | PinGroup::Pwm0
            | PinGroup::Pwm1
            | PinGroup::Pwm2
            | PinGroup::Pwm3
            | PinGroup::Sdio
    )
}

fn gpio_updates(group: PinGroup) -> Vec<MuxUpdate> {
    use PinGroup as G;
    match group {
        G::SataLed => vec![MuxUpdate::bit(0, 8, false)],
        G::Can0 => vec![MuxUpdate::bit(0, 16, false)],
        G::Can1 => vec![MuxUpdate::bit(0, 17, false)],
        G::HdaI2s => vec![MuxUpdate::bit(0, 6, false), MuxUpdate::bit(0, 4, false)],
        G::I2c0 => vec![MuxUpdate::bit(0, 10, false)],
        G::I2c1 => vec![MuxUpdate::bit(0, 11, false)],
        G::Nand => vec![MuxUpdate::bit(0, 9, false)],
        G::Pwm0 => vec![MuxUpdate::bit(0, 12, false)],
        G::Pwm1 => vec![MuxUpdate::bit(0, 13, false)],
        G::Pwm2 => vec![MuxUpdate::bit(0, 14, false)],
        G::Pwm3 => vec![MuxUpdate::bit(0, 15, false)],
        G::Sdio => vec![MuxUpdate::bit(0, 20, false)],
        _ => Vec::new(),
    }
}

fn dvo_lio_updates(dvo: bool) -> Vec<MuxUpdate> {
    vec![
        MuxUpdate::bit(0x10, 1, dvo),
        MuxUpdate::bit(0x08, 12, false),
        MuxUpdate::bit(0x08, 13, false),
        MuxUpdate::bit(0x00, 7, !dvo),
    ]
}

fn uart_updates(uart1: bool, mode: u32) -> Vec<MuxUpdate> {
    let select_bit = if uart1 { 12 } else { 13 };
    let mode_shift = if uart1 { 4 } else { 8 };
    vec![
        MuxUpdate::bit(0x10, 1, false),
        MuxUpdate::bit(0x08, select_bit, true),
        MuxUpdate::bit(0x00, 7, false),
        MuxUpdate::field(0x08, mode_shift, 4, mode),
    ]
}
