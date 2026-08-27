const SYS0_OFFSET: usize = 0x00;
const SYS1_OFFSET: usize = 0x08;
const DDR0_OFFSET: usize = 0x10;
const DDR1_OFFSET: usize = 0x18;
const DC0_OFFSET: usize = 0x20;
const DC1_OFFSET: usize = 0x28;
const PIX00_OFFSET: usize = 0x30;
const PIX01_OFFSET: usize = 0x38;
const PIX10_OFFSET: usize = 0x40;
const PIX11_OFFSET: usize = 0x48;
const FREQ_SCALE_OFFSET: usize = 0x50;
const REQUIRED_WINDOW_SIZE: usize = FREQ_SCALE_OFFSET + core::mem::size_of::<u64>();
const LS2K1000_FIRMWARE_WINDOW_SIZE: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ls2kClockMmioLayoutError {
    Unaligned,
    WindowTooSmall,
    AddressOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ls2kClockRegisters {
    pub(crate) sys0: usize,
    pub(crate) sys1: usize,
    pub(crate) ddr0: usize,
    pub(crate) ddr1: usize,
    pub(crate) dc0: usize,
    pub(crate) dc1: usize,
    pub(crate) pix00: usize,
    pub(crate) pix01: usize,
    pub(crate) pix10: usize,
    pub(crate) pix11: usize,
    pub(crate) freq_scale: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ls2kClockMmioLayout {
    base: usize,
}

impl Ls2kClockMmioLayout {
    pub(crate) fn new(base: usize, size: usize) -> Result<Self, Ls2kClockMmioLayoutError> {
        if !base.is_multiple_of(core::mem::align_of::<u64>()) {
            return Err(Ls2kClockMmioLayoutError::Unaligned);
        }
        // 现有 LS2K1000 固件把 SYS0 窗口长度错误地写成 1；该例外只由精确
        // 兼容串匹配的驱动采用，其余短窗口仍必须拒绝。
        if size != LS2K1000_FIRMWARE_WINDOW_SIZE && size < REQUIRED_WINDOW_SIZE {
            return Err(Ls2kClockMmioLayoutError::WindowTooSmall);
        }
        base.checked_add(REQUIRED_WINDOW_SIZE)
            .ok_or(Ls2kClockMmioLayoutError::AddressOverflow)?;
        Ok(Self { base })
    }

    pub(crate) const fn registers(self) -> Ls2kClockRegisters {
        Ls2kClockRegisters {
            sys0: self.base + SYS0_OFFSET,
            sys1: self.base + SYS1_OFFSET,
            ddr0: self.base + DDR0_OFFSET,
            ddr1: self.base + DDR1_OFFSET,
            dc0: self.base + DC0_OFFSET,
            dc1: self.base + DC1_OFFSET,
            pix00: self.base + PIX00_OFFSET,
            pix01: self.base + PIX01_OFFSET,
            pix10: self.base + PIX10_OFFSET,
            pix11: self.base + PIX11_OFFSET,
            freq_scale: self.base + FREQ_SCALE_OFFSET,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum ClockId {
    Ref = 0,
    Node = 1,
    Cpu = 2,
    Ddr = 3,
    Gpu = 4,
    Hda = 5,
    Dc = 6,
    Pix0 = 7,
    Pix1 = 8,
    Gmac = 9,
    Sata = 10,
    Usb = 11,
    Apb = 12,
    Spi = 13,
    I2sMclk = 14,
}

impl TryFrom<u32> for ClockId {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Ref,
            1 => Self::Node,
            2 => Self::Cpu,
            3 => Self::Ddr,
            4 => Self::Gpu,
            5 => Self::Hda,
            6 => Self::Dc,
            7 => Self::Pix0,
            8 => Self::Pix1,
            9 => Self::Gmac,
            10 => Self::Sata,
            11 => Self::Usb,
            12 => Self::Apb,
            13 => Self::Spi,
            14 => Self::I2sMclk,
            _ => return Err(()),
        })
    }
}

pub(crate) fn clock_id_from_specifier(specifier: &[u32]) -> Result<ClockId, ()> {
    let [id] = specifier else {
        return Err(());
    };
    ClockId::try_from(*id)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ls2kClockSnapshot {
    pub(crate) sys0: u64,
    pub(crate) sys1: u64,
    pub(crate) ddr0: u64,
    pub(crate) ddr1: u64,
    pub(crate) dc0: u64,
    pub(crate) dc1: u64,
    pub(crate) pix00: u64,
    pub(crate) pix01: u64,
    pub(crate) pix10: u64,
    pub(crate) pix11: u64,
    pub(crate) freq_scale: u64,
}

impl Ls2kClockSnapshot {
    pub(crate) fn rate(self, clock: ClockId, parent_rate: u64) -> Option<u64> {
        if parent_rate == 0 {
            return None;
        }
        match clock {
            ClockId::Ref | ClockId::Spi => Some(parent_rate),
            ClockId::Node => pll_rate(parent_rate, self.sys0, self.sys1, 0, 0x3f),
            ClockId::Cpu => {
                let node = self.rate(ClockId::Node, parent_rate)?;
                scaled_rate(node, field(self.freq_scale, 0, 0x7))
            }
            ClockId::Ddr => pll_rate(parent_rate, self.ddr0, self.ddr1, 0, 0x3f),
            ClockId::Gpu => pll_rate(parent_rate, self.ddr0, self.ddr1, 22, 0x3f),
            ClockId::Hda => pll_rate(parent_rate, self.ddr0, self.ddr1, 44, 0x7f),
            ClockId::Dc => pll_rate(parent_rate, self.dc0, self.dc1, 0, 0x3f),
            ClockId::Pix0 => pll_rate(parent_rate, self.pix00, self.pix01, 0, 0x3f),
            ClockId::Pix1 => pll_rate(parent_rate, self.pix10, self.pix11, 0, 0x3f),
            ClockId::Gmac => pll_rate(parent_rate, self.dc0, self.dc1, 22, 0x3f),
            ClockId::Sata => {
                let gmac = self.rate(ClockId::Gmac, parent_rate)?;
                scaled_rate(gmac, field(self.freq_scale, 12, 0x7))
            }
            ClockId::Usb => {
                let gmac = self.rate(ClockId::Gmac, parent_rate)?;
                scaled_rate(gmac, field(self.freq_scale, 16, 0x7))
            }
            ClockId::Apb => {
                let gmac = self.rate(ClockId::Gmac, parent_rate)?;
                scaled_rate(gmac, field(self.freq_scale, 20, 0x7))
            }
            ClockId::I2sMclk => None,
        }
    }
}

const fn field(value: u64, shift: u32, mask: u64) -> u64 {
    (value >> shift) & mask
}

fn pll_rate(
    parent_rate: u64,
    pll_control: u64,
    output_control: u64,
    output_shift: u32,
    output_mask: u64,
) -> Option<u64> {
    let loop_multiplier = field(pll_control, 32, 0x3ff);
    let reference_divisor = field(pll_control, 26, 0x3f);
    let output_divisor = field(output_control, output_shift, output_mask);
    let divisor = reference_divisor.checked_mul(output_divisor)?;
    mul_div_floor(parent_rate, loop_multiplier, divisor)
}

fn scaled_rate(parent_rate: u64, scale: u64) -> Option<u64> {
    mul_div_floor(parent_rate, scale.checked_add(1)?, 8)
}

fn mul_div_floor(value: u64, multiplier: u64, divisor: u64) -> Option<u64> {
    if multiplier == 0 || divisor == 0 {
        return None;
    }
    let quotient = value / divisor;
    let remainder = value % divisor;
    quotient
        .checked_mul(multiplier)?
        .checked_add(remainder.checked_mul(multiplier)?.checked_div(divisor)?)
}
