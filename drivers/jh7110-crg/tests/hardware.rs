#[path = "../src/hardware.rs"]
mod hardware;

use core::cell::Cell;

use hardware::{Registers, StgError, set_stg_clock, set_stg_reset};

const CLOCK_HCLK: usize = 0x3c;
const CLOCK_AHB: usize = 0x40;
const RESET_ASSERT: usize = 0x74;
const RESET_STATUS: usize = 0x78;

struct FakeRegisters {
    hclk: Cell<u32>,
    ahb: Cell<u32>,
    reset_assert: Cell<u32>,
    reset_status: Cell<u32>,
}

impl FakeRegisters {
    fn new() -> Self {
        Self {
            hclk: Cell::new(0x0012_3456),
            ahb: Cell::new(0x0065_4321),
            reset_assert: Cell::new(1 << 3),
            reset_status: Cell::new(1 << 3),
        }
    }
}

impl Registers for FakeRegisters {
    fn read32(&self, offset: usize) -> u32 {
        match offset {
            CLOCK_HCLK => self.hclk.get(),
            CLOCK_AHB => self.ahb.get(),
            RESET_ASSERT => self.reset_assert.get(),
            RESET_STATUS => self.reset_status.get(),
            _ => 0,
        }
    }

    fn write32(&self, offset: usize, value: u32) {
        match offset {
            CLOCK_HCLK => self.hclk.set(value),
            CLOCK_AHB => self.ahb.set(value),
            RESET_ASSERT => self.reset_assert.set(value),
            _ => {}
        }
    }

    fn relax(&self) {}
}

#[test]
fn enabling_trng_clocks_sets_only_the_gate_bit() {
    let registers = FakeRegisters::new();

    assert_eq!(set_stg_clock(&registers, 15, true), Ok(()));
    assert_eq!(set_stg_clock(&registers, 16, true), Ok(()));
    assert_eq!(registers.hclk.get(), 0x8012_3456);
    assert_eq!(registers.ahb.get(), 0x8065_4321);
}

#[test]
fn disabling_trng_clock_clears_only_the_gate_bit() {
    let registers = FakeRegisters::new();
    registers.hclk.set(0x8012_3456);

    assert_eq!(set_stg_clock(&registers, 15, false), Ok(()));
    assert_eq!(registers.hclk.get(), 0x0012_3456);
}

#[test]
fn unsupported_stg_clock_is_rejected_without_writes() {
    let registers = FakeRegisters::new();

    assert_eq!(
        set_stg_clock(&registers, 14, true),
        Err(StgError::Unsupported)
    );
    assert_eq!(registers.hclk.get(), 0x0012_3456);
    assert_eq!(registers.ahb.get(), 0x0065_4321);
}

#[test]
fn deasserting_security_reset_clears_only_bit_three() {
    let registers = FakeRegisters::new();
    registers.reset_assert.set(0xa5a5_0008);

    assert_eq!(set_stg_reset(&registers, 3, false, 4), Ok(()));
    assert_eq!(registers.reset_assert.get(), 0xa5a5_0000);
}

#[test]
fn reset_transition_times_out_when_status_never_changes() {
    let registers = FakeRegisters::new();
    registers.reset_status.set(0);

    assert_eq!(
        set_stg_reset(&registers, 3, false, 3),
        Err(StgError::Timeout)
    );
}
