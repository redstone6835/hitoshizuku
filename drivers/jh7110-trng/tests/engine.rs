#[path = "../src/engine.rs"]
mod engine;
#[path = "../src/status.rs"]
mod status;

use core::cell::Cell;

use engine::{Registers, TrngError, read_seed};

const REG_CTRL: usize = 0x00;
const REG_STAT: usize = 0x04;
const REG_ISTAT: usize = 0x14;
const REG_RAND0: usize = 0x20;

const CMD_NOP: u32 = 0;
const CMD_GENERATE: u32 = 1;
const CMD_RESEED: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Reseed,
    Generate,
}

struct FakeRegisters {
    phase: Cell<Phase>,
    status: u32,
    reseed_interrupt: u32,
    generate_interrupt: u32,
    words: [u32; 8],
}

impl FakeRegisters {
    fn ready(words: [u32; 8]) -> Self {
        Self {
            phase: Cell::new(Phase::Idle),
            status: 0,
            reseed_interrupt: 1 << 1,
            generate_interrupt: 1 << 0,
            words,
        }
    }
}

impl Registers for FakeRegisters {
    fn read32(&self, offset: usize) -> u32 {
        match offset {
            REG_STAT => self.status,
            REG_ISTAT => match self.phase.get() {
                Phase::Idle => 0,
                Phase::Reseed => self.reseed_interrupt,
                Phase::Generate => self.generate_interrupt,
            },
            REG_RAND0..=0x3c => self.words[(offset - REG_RAND0) / 4],
            _ => 0,
        }
    }

    fn write32(&self, offset: usize, value: u32) {
        if offset != REG_CTRL {
            return;
        }
        match value {
            CMD_NOP => self.phase.set(Phase::Idle),
            CMD_RESEED => self.phase.set(Phase::Reseed),
            CMD_GENERATE => self.phase.set(Phase::Generate),
            _ => {}
        }
    }

    fn relax(&self) {}
}

#[test]
fn successful_transaction_returns_all_256_bits() {
    let words = [
        0x0302_0100,
        0x0706_0504,
        0x0b0a_0908,
        0x0f0e_0d0c,
        0x1312_1110,
        0x1716_1514,
        0x1b1a_1918,
        0x1f1e_1d1c,
    ];
    let registers = FakeRegisters::ready(words);

    assert_eq!(
        read_seed(&registers, 4),
        Ok([
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31,
        ])
    );
}

#[test]
fn busy_controller_times_out_without_starting_transaction() {
    let mut registers = FakeRegisters::ready([0; 8]);
    registers.status = 1 << 30;

    assert_eq!(read_seed(&registers, 3), Err(TrngError::Timeout));
    assert_eq!(registers.phase.get(), Phase::Idle);
}

#[test]
fn lfsr_lockup_during_reseed_rejects_seed() {
    let mut registers = FakeRegisters::ready([0xaaaa_aaaa; 8]);
    registers.reseed_interrupt = (1 << 1) | (1 << 4);

    assert_eq!(read_seed(&registers, 3), Err(TrngError::Lockup));
}

#[test]
fn missing_random_ready_times_out_without_returning_data() {
    let mut registers = FakeRegisters::ready([0x5555_5555; 8]);
    registers.generate_interrupt = 0;

    assert_eq!(read_seed(&registers, 3), Err(TrngError::Timeout));
}
