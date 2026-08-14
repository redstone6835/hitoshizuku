const SOURCE_COUNT: u32 = 64;
const CONTROL_DECLARED_SIZE: usize = 0x40;
const ISR_DECLARED_SIZE: usize = 0x10;
const BANK_STRIDE: usize = 0x40;
const SOURCES_PER_BANK: u32 = 32;
const ROUTE_OFFSET: usize = 0x00;
const ENABLE_OFFSET: usize = 0x28;
const DISABLE_OFFSET: usize = 0x2c;
const POLARITY_OFFSET: usize = 0x30;
const EDGE_OFFSET: usize = 0x34;
const BOUNCE_OFFSET: usize = 0x38;
const AUTO_OFFSET: usize = 0x3c;
const CONTROL_HARDWARE_SIZE: usize = BANK_STRIDE * 2;
const CORE_ISR_STRIDE: usize = 0x100;
const HIGH_ISR_OFFSET: usize = 0x08;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ls2kIcuLayoutError {
    Unaligned,
    ControlWindowTooSmall,
    IsrWindowTooSmall,
    AddressOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ls2kIcuSourceRegs {
    pub(crate) route: usize,
    pub(crate) enable: usize,
    pub(crate) disable: usize,
    pub(crate) polarity: usize,
    pub(crate) edge: usize,
    pub(crate) bounce: usize,
    pub(crate) auto: usize,
    pub(crate) bit: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ls2kIcuLayout {
    control_base: usize,
    isr_base: usize,
}

pub(crate) struct Ls2kPendingSources {
    words: [u32; 2],
    bank: usize,
}

impl Iterator for Ls2kPendingSources {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        while self.bank < self.words.len() {
            let word = &mut self.words[self.bank];
            if *word != 0 {
                let bit = word.trailing_zeros();
                *word &= !(1u32 << bit);
                return Some(self.bank as u32 * SOURCES_PER_BANK + bit);
            }
            self.bank += 1;
        }
        None
    }
}

pub(crate) const fn pending_sources(words: [u32; 2]) -> Ls2kPendingSources {
    Ls2kPendingSources { words, bank: 0 }
}

pub(crate) const fn route_value(parent_line: usize, core_mask: u8) -> Option<u8> {
    if parent_line >= 4 || core_mask == 0 || core_mask > 0x0f {
        return None;
    }
    Some((1u8 << (parent_line + 4)) | core_mask)
}

impl Ls2kIcuLayout {
    pub(crate) fn new(
        control_base: usize,
        control_size: usize,
        isr_base: usize,
        isr_size: usize,
    ) -> Result<Self, Ls2kIcuLayoutError> {
        if !control_base.is_multiple_of(4) || !isr_base.is_multiple_of(8) {
            return Err(Ls2kIcuLayoutError::Unaligned);
        }
        if control_size < CONTROL_DECLARED_SIZE {
            return Err(Ls2kIcuLayoutError::ControlWindowTooSmall);
        }
        if isr_size < ISR_DECLARED_SIZE {
            return Err(Ls2kIcuLayoutError::IsrWindowTooSmall);
        }
        control_base
            .checked_add(control_size.max(CONTROL_HARDWARE_SIZE))
            .ok_or(Ls2kIcuLayoutError::AddressOverflow)?;
        isr_base
            .checked_add(isr_size)
            .ok_or(Ls2kIcuLayoutError::AddressOverflow)?;
        Ok(Self {
            control_base,
            isr_base,
        })
    }

    pub(crate) fn source(self, source: u32) -> Option<Ls2kIcuSourceRegs> {
        if source >= SOURCE_COUNT {
            return None;
        }
        let bank = (source / SOURCES_PER_BANK) as usize;
        let index = source % SOURCES_PER_BANK;
        let bank_base = self.control_base + bank * BANK_STRIDE;
        Some(Ls2kIcuSourceRegs {
            route: bank_base + ROUTE_OFFSET + index as usize,
            enable: bank_base + ENABLE_OFFSET,
            disable: bank_base + DISABLE_OFFSET,
            polarity: bank_base + POLARITY_OFFSET,
            edge: bank_base + EDGE_OFFSET,
            bounce: bank_base + BOUNCE_OFFSET,
            auto: bank_base + AUTO_OFFSET,
            bit: 1u32 << index,
        })
    }

    pub(crate) fn pending(self, core: usize) -> Option<[usize; 2]> {
        let low = self
            .isr_base
            .checked_add(core.checked_mul(CORE_ISR_STRIDE)?)?;
        Some([low, low.checked_add(HIGH_ISR_OFFSET)?])
    }
}
