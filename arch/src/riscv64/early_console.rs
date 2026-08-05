//! RISC-V64 最早期 16550 控制台。
//!
//! 在正式设备模型建立前直接轮询 UART。启动时先使用 QEMU virt 兜底配置；DTB
//! 可用后原子切换为 `/chosen/stdout-path` 的完整寄存器布局，正式 MMIO 页表发布后
//! 再从低地址 identity mapping 切到高半区窗口。

use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use fdt::Fdt;

use crate::early_console_config::{
    EarlyUartConfig, EarlyUartConfigError, RegisterEndian, RegisterIoWidth,
    early_uart_config_from_fdt,
};

const EARLY_MMIO_PHYS_END: usize = 0x4000_0000;

const REG_THR: usize = 0;
const REG_IER: usize = 1;
const REG_FCR: usize = 2;
const REG_LCR: usize = 3;
const REG_MCR: usize = 4;
const REG_LSR: usize = 5;

const UART_LSR_THRE: u8 = 1 << 5;
const UART_LCR_DLAB: u8 = 1 << 7;
const UART_LCR_8N1: u8 = 0x03;
const UART_FCR_ENABLE_CLEAR: u8 = 0x07;
const UART_MCR_DTR_RTS: u8 = 0x03;

const UART_STATE_UNINITIALIZED: usize = 0;
const UART_STATE_INITIALIZING: usize = 1;
const UART_STATE_READY: usize = 2;

const FALLBACK_EARLY_UART_CONFIG: EarlyUartConfig = EarlyUartConfig {
    phys_base: 0x1000_0000,
    clock_hz: 1_843_200,
    baud: 115_200,
    reg_offset: 0,
    reg_shift: 0,
    io_width: RegisterIoWidth::U8,
    endian: RegisterEndian::Little,
};

static UART_PHYS_BASE: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.phys_base);
static UART_BASE: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.phys_base);
static UART_CLOCK_HZ: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.clock_hz as usize);
static UART_BAUD: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.baud as usize);
static UART_REG_OFFSET: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.reg_offset);
static UART_REG_SHIFT: AtomicUsize =
    AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.reg_shift as usize);
static UART_IO_WIDTH: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.io_width.bytes());
static UART_ENDIAN: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.endian as usize);
static UART_STATE: AtomicUsize = AtomicUsize::new(UART_STATE_UNINITIALIZED);

/// DTB 可用后采用 chosen UART 的完整 binding 配置。
pub(crate) fn configure_from_dtb(dtb: Fdt<'_>) -> Result<EarlyUartConfig, EarlyUartConfigError> {
    let config = early_uart_config_from_fdt(dtb)?;
    let end = config
        .register_offset(REG_LSR)
        .and_then(|offset| offset.checked_add(config.io_width.bytes()))
        .and_then(|span| config.phys_base.checked_add(span))
        .ok_or(EarlyUartConfigError::AddressOverflow)?;
    if config.phys_base == 0 || end > EARLY_MMIO_PHYS_END {
        return Err(EarlyUartConfigError::AddressOverflow);
    }

    while UART_STATE.load(Ordering::Acquire) == UART_STATE_INITIALIZING {
        core::hint::spin_loop();
    }
    UART_CLOCK_HZ.store(config.clock_hz as usize, Ordering::Relaxed);
    UART_BAUD.store(config.baud as usize, Ordering::Relaxed);
    UART_REG_OFFSET.store(config.reg_offset, Ordering::Relaxed);
    UART_REG_SHIFT.store(config.reg_shift as usize, Ordering::Relaxed);
    UART_IO_WIDTH.store(config.io_width.bytes(), Ordering::Relaxed);
    UART_ENDIAN.store(config.endian as usize, Ordering::Relaxed);
    UART_PHYS_BASE.store(config.phys_base, Ordering::Relaxed);
    UART_BASE.store(config.phys_base, Ordering::Release);
    UART_STATE.store(UART_STATE_UNINITIALIZED, Ordering::Release);
    Ok(config)
}

/// 正式 MMIO 映射建立后切到高半区地址。
pub fn switch_to_virtual() {
    let vaddr = UART_PHYS_BASE
        .load(Ordering::Acquire)
        .wrapping_add(crate::riscv64::heap_vm::MMIO_VIRT_BASE);
    UART_BASE.store(vaddr, Ordering::Release);
}

pub fn e_print(args: core::fmt::Arguments) {
    let _ = ConsoleWriter.write_fmt(args);
}

pub fn e_write_bytes(bytes: &[u8]) {
    console_write_bytes(bytes);
}

fn load_config() -> EarlyUartConfig {
    let _ = UART_BASE.load(Ordering::Acquire);
    let io_width = match UART_IO_WIDTH.load(Ordering::Relaxed) {
        2 => RegisterIoWidth::U16,
        4 => RegisterIoWidth::U32,
        _ => RegisterIoWidth::U8,
    };
    let endian = match UART_ENDIAN.load(Ordering::Relaxed) {
        value if value == RegisterEndian::Big as usize => RegisterEndian::Big,
        _ => RegisterEndian::Little,
    };
    EarlyUartConfig {
        phys_base: UART_PHYS_BASE.load(Ordering::Relaxed),
        clock_hz: UART_CLOCK_HZ.load(Ordering::Relaxed) as u32,
        baud: UART_BAUD.load(Ordering::Relaxed) as u32,
        reg_offset: UART_REG_OFFSET.load(Ordering::Relaxed),
        reg_shift: UART_REG_SHIFT.load(Ordering::Relaxed) as u32,
        io_width,
        endian,
    }
}

fn register_address(config: EarlyUartConfig, register: usize) -> usize {
    let offset = config
        .register_offset(register)
        .expect("validated early UART register shift");
    UART_BASE
        .load(Ordering::Acquire)
        .checked_add(offset)
        .expect("validated early UART register address")
}

fn write_register(config: EarlyUartConfig, register: usize, value: u8) {
    let address = register_address(config, register);
    // Safety: DT 配置在发布前已校验地址、步长、对齐与访问宽度；目标地址处于当前
    // early identity 或正式高半区 MMIO 映射中，只执行易失设备访问。
    unsafe {
        match config.io_width {
            RegisterIoWidth::U8 => core::ptr::write_volatile(address as *mut u8, value),
            RegisterIoWidth::U16 => {
                let value = match config.endian {
                    RegisterEndian::Little => u16::from(value).to_le(),
                    RegisterEndian::Big => u16::from(value).to_be(),
                };
                core::ptr::write_volatile(address as *mut u16, value);
            }
            RegisterIoWidth::U32 => {
                let value = match config.endian {
                    RegisterEndian::Little => u32::from(value).to_le(),
                    RegisterEndian::Big => u32::from(value).to_be(),
                };
                core::ptr::write_volatile(address as *mut u32, value);
            }
        }
    }
}

fn read_register(config: EarlyUartConfig, register: usize) -> u8 {
    let address = register_address(config, register);
    // Safety: 与 `write_register` 相同；多字节值按 binding 端序还原后只取 UART
    // 寄存器定义的低 8 位。
    unsafe {
        match config.io_width {
            RegisterIoWidth::U8 => core::ptr::read_volatile(address as *const u8),
            RegisterIoWidth::U16 => {
                let value = core::ptr::read_volatile(address as *const u16);
                match config.endian {
                    RegisterEndian::Little => u16::from_le(value) as u8,
                    RegisterEndian::Big => u16::from_be(value) as u8,
                }
            }
            RegisterIoWidth::U32 => {
                let value = core::ptr::read_volatile(address as *const u32);
                match config.endian {
                    RegisterEndian::Little => u32::from_le(value) as u8,
                    RegisterEndian::Big => u32::from_be(value) as u8,
                }
            }
        }
    }
}

fn ensure_initialized(config: EarlyUartConfig) {
    match UART_STATE.compare_exchange(
        UART_STATE_UNINITIALIZED,
        UART_STATE_INITIALIZING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            let divisor = config
                .divisor()
                .expect("published early UART config must have a valid divisor");
            write_register(config, REG_IER, 0);
            write_register(config, REG_LCR, UART_LCR_DLAB);
            write_register(config, REG_THR, divisor as u8);
            write_register(config, REG_IER, (divisor >> 8) as u8);
            write_register(config, REG_LCR, UART_LCR_8N1);
            write_register(config, REG_FCR, UART_FCR_ENABLE_CLEAR);
            write_register(config, REG_MCR, UART_MCR_DTR_RTS);
            UART_STATE.store(UART_STATE_READY, Ordering::Release);
        }
        Err(UART_STATE_INITIALIZING) => {
            while UART_STATE.load(Ordering::Acquire) != UART_STATE_READY {
                core::hint::spin_loop();
            }
        }
        Err(UART_STATE_READY) => {}
        Err(_) => unreachable!("early UART state has a closed value set"),
    }
}

fn console_write_bytes(bytes: &[u8]) {
    let config = load_config();
    ensure_initialized(config);
    for &byte in bytes {
        while read_register(config, REG_LSR) & UART_LSR_THRE == 0 {
            core::hint::spin_loop();
        }
        write_register(config, REG_THR, byte);
    }
}

struct ConsoleWriter;

impl Write for ConsoleWriter {
    fn write_str(&mut self, value: &str) -> core::fmt::Result {
        console_write_bytes(value.as_bytes());
        Ok(())
    }
}
