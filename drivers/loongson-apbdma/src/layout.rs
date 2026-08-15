//! LS2X APB-DMA 的 MMIO 布局和控制字编码。

const ORDER_CONFIG_MASK: u64 = 0x1f;
const ORDER_64_BIT_ENABLE: u64 = 1;
const ORDER_START: u64 = 1 << 3;
const ORDER_STOP: u64 = 1 << 4;
const ORDER_ALIGNMENT: u64 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ls2xDmaMmioLayoutError {
    Unaligned,
    WindowTooSmall,
    AddressOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ls2xDmaMmioLayout {
    order: usize,
}

impl Ls2xDmaMmioLayout {
    pub(crate) fn new(base: usize, size: usize) -> Result<Self, Ls2xDmaMmioLayoutError> {
        if !base.is_multiple_of(8) {
            return Err(Ls2xDmaMmioLayoutError::Unaligned);
        }
        if size < 8 {
            return Err(Ls2xDmaMmioLayoutError::WindowTooSmall);
        }
        base.checked_add(7)
            .ok_or(Ls2xDmaMmioLayoutError::AddressOverflow)?;
        Ok(Self { order: base })
    }

    pub(crate) const fn order(self) -> usize {
        self.order
    }
}

pub(crate) fn start_order(
    descriptor: u64,
    address_64_bit: bool,
) -> Result<u64, Ls2xDmaMmioLayoutError> {
    if !descriptor.is_multiple_of(ORDER_ALIGNMENT) {
        return Err(Ls2xDmaMmioLayoutError::Unaligned);
    }
    Ok(descriptor
        | ORDER_START
        | if address_64_bit {
            ORDER_64_BIT_ENABLE
        } else {
            0
        })
}

pub(crate) const fn stop_order(current: u64, address_64_bit: bool) -> u64 {
    (current & !ORDER_CONFIG_MASK)
        | ORDER_STOP
        | if address_64_bit {
            ORDER_64_BIT_ENABLE
        } else {
            0
        }
}

pub(crate) const fn apply_selector(current: u64, bit: u32, value: bool) -> Option<u64> {
    if bit >= 64 {
        return None;
    }
    let mask = 1u64 << bit;
    Some(if value {
        current | mask
    } else {
        current & !mask
    })
}
