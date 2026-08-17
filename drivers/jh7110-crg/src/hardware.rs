const CLOCK_ENABLE: u32 = 1 << 31;
const CLOCK_HCLK: usize = 0x3c;
const CLOCK_AHB: usize = 0x40;
const RESET_ASSERT: usize = 0x74;
const RESET_STATUS: usize = 0x78;
const SECURITY_RESET_MASK: u32 = 1 << 3;

pub(crate) trait Registers {
    fn read32(&self, offset: usize) -> u32;
    fn write32(&self, offset: usize, value: u32);
    fn relax(&self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StgError {
    Unsupported,
    Timeout,
}

fn clock_offset(id: u32) -> Option<usize> {
    match id {
        15 => Some(CLOCK_HCLK),
        16 => Some(CLOCK_AHB),
        _ => None,
    }
}

pub(crate) fn set_stg_clock(
    registers: &impl Registers,
    id: u32,
    enable: bool,
) -> Result<(), StgError> {
    let offset = clock_offset(id).ok_or(StgError::Unsupported)?;
    let current = registers.read32(offset);
    let next = if enable {
        current | CLOCK_ENABLE
    } else {
        current & !CLOCK_ENABLE
    };
    registers.write32(offset, next);
    Ok(())
}

pub(crate) fn set_stg_reset(
    registers: &impl Registers,
    id: u32,
    assert: bool,
    max_polls: usize,
) -> Result<(), StgError> {
    if id != 3 {
        return Err(StgError::Unsupported);
    }

    let current = registers.read32(RESET_ASSERT);
    let next = if assert {
        current | SECURITY_RESET_MASK
    } else {
        current & !SECURITY_RESET_MASK
    };
    registers.write32(RESET_ASSERT, next);

    for _ in 0..max_polls {
        let deasserted = registers.read32(RESET_STATUS) & SECURITY_RESET_MASK != 0;
        if deasserted == !assert {
            return Ok(());
        }
        registers.relax();
    }
    Err(StgError::Timeout)
}
