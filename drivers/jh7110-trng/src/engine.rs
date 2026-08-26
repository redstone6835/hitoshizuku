use crate::status::{WaitState, is_idle, random_wait_state, reseed_wait_state};

const REG_CTRL: usize = 0x00;
const REG_STAT: usize = 0x04;
const REG_MODE: usize = 0x08;
const REG_SMODE: usize = 0x0c;
const REG_IE: usize = 0x10;
const REG_ISTAT: usize = 0x14;
const REG_RAND0: usize = 0x20;
const REG_AUTO_REQUESTS: usize = 0x60;
const REG_AUTO_AGE: usize = 0x64;

const CTRL_NOP: u32 = 0;
const CTRL_GENERATE: u32 = 1;
const CTRL_RESEED: u32 = 2;

const MODE_256_BITS: u32 = 1 << 3;
const IE_RANDOM_READY: u32 = 1 << 0;
const IE_SEED_DONE: u32 = 1 << 1;
const IE_LFSR_LOCKUP: u32 = 1 << 4;
const IE_GLOBAL: u32 = 1 << 31;
const IE_ALL: u32 = IE_GLOBAL | IE_RANDOM_READY | IE_SEED_DONE | IE_LFSR_LOCKUP;

pub(crate) trait Registers {
    fn read32(&self, offset: usize) -> u32;
    fn write32(&self, offset: usize, value: u32);
    fn relax(&self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrngError {
    Timeout,
    Lockup,
}

fn wait_idle(registers: &impl Registers, max_polls: usize) -> Result<(), TrngError> {
    for _ in 0..max_polls {
        if is_idle(registers.read32(REG_STAT)) {
            return Ok(());
        }
        registers.relax();
    }
    Err(TrngError::Timeout)
}

fn wait_event(
    registers: &impl Registers,
    max_polls: usize,
    state: fn(u32) -> WaitState,
) -> Result<(), TrngError> {
    for _ in 0..max_polls {
        match state(registers.read32(REG_ISTAT)) {
            WaitState::Ready => return Ok(()),
            WaitState::Lockup => return Err(TrngError::Lockup),
            WaitState::Pending => registers.relax(),
        }
    }
    Err(TrngError::Timeout)
}

pub(crate) fn read_seed(
    registers: &impl Registers,
    max_polls: usize,
) -> Result<[u8; 32], TrngError> {
    registers.write32(REG_AUTO_AGE, 0);
    registers.write32(REG_AUTO_REQUESTS, 0);
    let pending = registers.read32(REG_ISTAT);
    registers.write32(REG_ISTAT, pending);
    registers.write32(REG_IE, IE_ALL);
    registers.write32(REG_MODE, registers.read32(REG_MODE) | MODE_256_BITS);
    registers.write32(REG_SMODE, registers.read32(REG_SMODE));

    wait_idle(registers, max_polls)?;
    registers.write32(REG_CTRL, CTRL_NOP);
    wait_idle(registers, max_polls)?;

    registers.write32(REG_CTRL, CTRL_RESEED);
    wait_event(registers, max_polls, reseed_wait_state)?;
    registers.write32(REG_ISTAT, IE_SEED_DONE);
    wait_idle(registers, max_polls)?;

    registers.write32(REG_CTRL, CTRL_GENERATE);
    wait_event(registers, max_polls, random_wait_state)?;

    let mut seed = [0u8; 32];
    for (index, chunk) in seed.chunks_exact_mut(4).enumerate() {
        chunk.copy_from_slice(&registers.read32(REG_RAND0 + index * 4).to_le_bytes());
    }
    registers.write32(REG_ISTAT, IE_RANDOM_READY);
    Ok(seed)
}
