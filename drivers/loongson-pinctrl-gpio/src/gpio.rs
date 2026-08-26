use alloc::{boxed::Box, vec::Vec};

const REGISTER_WIDTH: usize = core::mem::size_of::<u64>();
const MAX_GPIO_LINES: u32 = u64::BITS;
const GPIO_ACTIVE_LOW: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GpioError {
    UnalignedWindow,
    InvalidLineCount,
    InvalidOffsets,
    RegisterOutsideWindow,
    AddressOverflow,
    InputBitOutOfRange,
    LineOutOfRange,
    InvalidSpecifier,
    UnsupportedFlags,
    InvalidIrqDescription,
    InterruptsUnsupported,
    LineBusy,
    LineNotAllocated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GpioLineAllocator {
    ngpios: u32,
    allocated: u64,
}

impl GpioLineAllocator {
    pub(crate) fn new(ngpios: u32) -> Result<Self, GpioError> {
        if ngpios == 0 || ngpios > MAX_GPIO_LINES {
            return Err(GpioError::InvalidLineCount);
        }
        Ok(Self {
            ngpios,
            allocated: 0,
        })
    }

    pub(crate) fn acquire(&mut self, line: u32) -> Result<(), GpioError> {
        let mask = self.mask(line)?;
        if self.allocated & mask != 0 {
            return Err(GpioError::LineBusy);
        }
        self.allocated |= mask;
        Ok(())
    }

    pub(crate) fn release(&mut self, line: u32) -> Result<(), GpioError> {
        let mask = self.mask(line)?;
        if self.allocated & mask == 0 {
            return Err(GpioError::LineNotAllocated);
        }
        self.allocated &= !mask;
        Ok(())
    }

    fn mask(self, line: u32) -> Result<u64, GpioError> {
        if line >= self.ngpios {
            return Err(GpioError::LineOutOfRange);
        }
        Ok(1u64 << line)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GpioOffsets {
    pub(crate) direction: usize,
    pub(crate) output: usize,
    pub(crate) input: usize,
    pub(crate) interrupt: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GpioLineLayout {
    pub(crate) direction_register: usize,
    pub(crate) output_register: usize,
    pub(crate) input_register: usize,
    pub(crate) interrupt_register: usize,
    pub(crate) direction_mask: u64,
    pub(crate) output_mask: u64,
    pub(crate) input_mask: u64,
    pub(crate) interrupt_mask: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegisterUpdate {
    pub(crate) address: usize,
    pub(crate) clear_mask: u64,
    pub(crate) set_mask: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GpioLayout {
    base: usize,
    size: usize,
    ngpios: u32,
    offsets: GpioOffsets,
    input_start: u32,
}

impl GpioLayout {
    pub(crate) fn new(
        base: usize,
        size: usize,
        ngpios: u32,
        offsets: GpioOffsets,
        input_start: u32,
    ) -> Result<Self, GpioError> {
        if !base.is_multiple_of(REGISTER_WIDTH) {
            return Err(GpioError::UnalignedWindow);
        }
        if ngpios == 0 || ngpios > MAX_GPIO_LINES {
            return Err(GpioError::InvalidLineCount);
        }
        if input_start
            .checked_add(ngpios)
            .is_none_or(|end| end > MAX_GPIO_LINES)
        {
            return Err(GpioError::InputBitOutOfRange);
        }
        base.checked_add(size).ok_or(GpioError::AddressOverflow)?;

        let raw_offsets = [
            offsets.direction,
            offsets.output,
            offsets.input,
            offsets.interrupt,
        ];
        for (index, &offset) in raw_offsets.iter().enumerate() {
            if !offset.is_multiple_of(REGISTER_WIDTH) {
                return Err(GpioError::InvalidOffsets);
            }
            if raw_offsets[..index].contains(&offset) {
                return Err(GpioError::InvalidOffsets);
            }
            if offset
                .checked_add(REGISTER_WIDTH)
                .is_none_or(|end| end > size)
            {
                return Err(GpioError::RegisterOutsideWindow);
            }
            base.checked_add(offset).ok_or(GpioError::AddressOverflow)?;
        }

        Ok(Self {
            base,
            size,
            ngpios,
            offsets,
            input_start,
        })
    }

    pub(crate) const fn ngpios(self) -> u32 {
        self.ngpios
    }

    #[cfg(test)]
    pub(crate) const fn size(self) -> usize {
        self.size
    }

    pub(crate) fn line(self, line: u32) -> Result<GpioLineLayout, GpioError> {
        if line >= self.ngpios {
            return Err(GpioError::LineOutOfRange);
        }
        let line_mask = 1u64 << line;
        let input_mask = 1u64 << (line + self.input_start);
        Ok(GpioLineLayout {
            direction_register: self.address(self.offsets.direction)?,
            output_register: self.address(self.offsets.output)?,
            input_register: self.address(self.offsets.input)?,
            interrupt_register: self.address(self.offsets.interrupt)?,
            direction_mask: line_mask,
            output_mask: line_mask,
            input_mask,
            interrupt_mask: line_mask,
        })
    }

    pub(crate) fn output_sequence(
        self,
        line: u32,
        high: bool,
    ) -> Result<[RegisterUpdate; 2], GpioError> {
        let line = self.line(line)?;
        let level = if high {
            RegisterUpdate {
                address: line.output_register,
                clear_mask: 0,
                set_mask: line.output_mask,
            }
        } else {
            RegisterUpdate {
                address: line.output_register,
                clear_mask: line.output_mask,
                set_mask: 0,
            }
        };
        let direction = RegisterUpdate {
            address: line.direction_register,
            clear_mask: line.direction_mask,
            set_mask: 0,
        };
        Ok([level, direction])
    }

    pub(crate) fn input_update(self, line: u32) -> Result<RegisterUpdate, GpioError> {
        let line = self.line(line)?;
        Ok(RegisterUpdate {
            address: line.direction_register,
            clear_mask: 0,
            set_mask: line.direction_mask,
        })
    }

    pub(crate) fn interrupt_update(
        self,
        line: u32,
        enabled: bool,
    ) -> Result<RegisterUpdate, GpioError> {
        let line = self.line(line)?;
        Ok(if enabled {
            RegisterUpdate {
                address: line.interrupt_register,
                clear_mask: 0,
                set_mask: line.interrupt_mask,
            }
        } else {
            RegisterUpdate {
                address: line.interrupt_register,
                clear_mask: line.interrupt_mask,
                set_mask: 0,
            }
        })
    }

    fn address(self, offset: usize) -> Result<usize, GpioError> {
        self.base
            .checked_add(offset)
            .ok_or(GpioError::AddressOverflow)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GpioSpecifier {
    pub(crate) line: u32,
    pub(crate) active_low: bool,
}

impl GpioSpecifier {
    pub(crate) fn decode(specifier: &[u32], ngpios: u32) -> Result<Self, GpioError> {
        let [line, flags] = specifier else {
            return Err(GpioError::InvalidSpecifier);
        };
        if *line >= ngpios {
            return Err(GpioError::LineOutOfRange);
        }
        if flags & !GPIO_ACTIVE_LOW != 0 {
            return Err(GpioError::UnsupportedFlags);
        }
        Ok(Self {
            line: *line,
            active_low: flags & GPIO_ACTIVE_LOW != 0,
        })
    }

    pub(crate) const fn physical_level(self, logical_high: bool) -> bool {
        logical_high ^ self.active_low
    }

    pub(crate) const fn logical_level(self, physical_high: bool) -> bool {
        physical_high ^ self.active_low
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpioIrqMap {
    ngpios: u32,
    sources: Box<[u32]>,
}

impl GpioIrqMap {
    pub(crate) fn new(ngpios: u32, sources: &[u32], support_irq: bool) -> Result<Self, GpioError> {
        if !support_irq {
            return Err(GpioError::InterruptsUnsupported);
        }
        // 固件可以只描述具备中断能力的 GPIO 前缀。没有对应 source 的高号
        // GPIO 仍然可以作为普通输入输出使用，只有超出 GPIO 总数的映射才无效。
        if ngpios == 0 || ngpios > MAX_GPIO_LINES || sources.len() > ngpios as usize {
            return Err(GpioError::InvalidIrqDescription);
        }
        let sources: Vec<u32> = sources.to_vec();
        Ok(Self {
            ngpios,
            sources: sources.into_boxed_slice(),
        })
    }

    pub(crate) fn has_source_for_line(&self, line: u32) -> bool {
        line < self.ngpios && (line as usize) < self.sources.len()
    }

    pub(crate) fn source_for_line(&self, line: u32) -> Result<u32, GpioError> {
        if line >= self.ngpios {
            return Err(GpioError::LineOutOfRange);
        }
        self.sources
            .get(line as usize)
            .copied()
            .ok_or(GpioError::InterruptsUnsupported)
    }
}
