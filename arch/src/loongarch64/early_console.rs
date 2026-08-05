//! LoongArch64 最早期串口输出。
//!
//! 正式设备模型和堆尚未建立时，本模块直接通过 DMW0 uncached 窗口访问 16550
//! 寄存器。UART 参数优先来自 EFI 配置表中的 FDT `/chosen/stdout-path`；零分配
//! 解析失败或固件尚不可用时，保留传统 QEMU/Loongson 参数作为安全兜底。

use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use fdt::Fdt;

use crate::early_console_config::{
    EarlyUartConfig, EarlyUartConfigError, FALLBACK_EARLY_UART_CONFIG, RegisterEndian,
    RegisterIoWidth, early_uart_config_from_fdt,
};

const DMW0_UNCACHED_BASE: usize = 0x8000_0000_0000_0000;

const REG_THR: usize = 0;
const REG_IER: usize = 1;
const REG_FCR: usize = 2;
const REG_LCR: usize = 3;
const REG_MCR: usize = 4;
const REG_LSR: usize = 5;

const UART_LSR_THRE: u8 = 1 << 5;
const UART_LCR_DLAB: u8 = 1 << 7;
const UART_LCR_8N1: u8 = 0x03;
const UART_FCR_ENABLE_FIFO: u8 = 0x01;
const UART_FCR_CLEAR_RXTX: u8 = 0x06;
const UART_MCR_DTR_RTS: u8 = 0x03;
const UART_FIFO_DEPTH: usize = 16;

const UART_STATE_UNINITIALIZED: usize = 0;
const UART_STATE_INITIALIZING: usize = 1;
const UART_STATE_READY: usize = 2;

static UART_PHYS_BASE: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.phys_base);
static UART_CLOCK_HZ: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.clock_hz as usize);
static UART_BAUD: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.baud as usize);
static UART_REG_OFFSET: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.reg_offset);
static UART_REG_SHIFT: AtomicUsize =
    AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.reg_shift as usize);
static UART_IO_WIDTH: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.io_width.bytes());
static UART_ENDIAN: AtomicUsize = AtomicUsize::new(FALLBACK_EARLY_UART_CONFIG.endian as usize);
static UART_SOURCE: AtomicUsize = AtomicUsize::new(EarlyConsoleSource::Fallback as usize);
static UART_STATE: AtomicUsize = AtomicUsize::new(UART_STATE_UNINITIALIZED);

/// 当前最早期控制台参数的来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum EarlyConsoleSource {
    Fallback = 0,
    DeviceTree = 1,
}

impl EarlyConsoleSource {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::DeviceTree => "dtb",
        }
    }
}

/// 启动代码可打印的控制台选择结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EarlyConsoleSelection {
    pub(crate) source: EarlyConsoleSource,
    pub(crate) config: EarlyUartConfig,
    pub(crate) dt_error: Option<EarlyUartConfigError>,
}

/// 在第一次输出前尝试采用 DT 控制台；失败时原子保留兜底配置。
pub(crate) fn configure_early_console(dtb: Option<Fdt<'_>>) -> EarlyConsoleSelection {
    let (candidate, source, dt_error) = match dtb {
        Some(dtb) => match early_uart_config_from_fdt(dtb) {
            Ok(config) if config_fits_uncached_window(config) => {
                (config, EarlyConsoleSource::DeviceTree, None)
            }
            Ok(_) => (
                FALLBACK_EARLY_UART_CONFIG,
                EarlyConsoleSource::Fallback,
                Some(EarlyUartConfigError::AddressOverflow),
            ),
            Err(error) => (
                FALLBACK_EARLY_UART_CONFIG,
                EarlyConsoleSource::Fallback,
                Some(error),
            ),
        },
        None => (
            FALLBACK_EARLY_UART_CONFIG,
            EarlyConsoleSource::Fallback,
            None,
        ),
    };

    if UART_STATE.load(Ordering::Acquire) == UART_STATE_UNINITIALIZED {
        store_config(candidate, source);
        EarlyConsoleSelection {
            source,
            config: candidate,
            dt_error,
        }
    } else {
        EarlyConsoleSelection {
            source: load_source(),
            config: load_config(),
            dt_error,
        }
    }
}

fn config_fits_uncached_window(config: EarlyUartConfig) -> bool {
    config
        .register_offset(REG_LSR)
        .and_then(|offset| offset.checked_add(config.io_width.bytes()))
        .and_then(|span| config.phys_base.checked_add(span))
        .is_some_and(|end| end <= DMW0_UNCACHED_BASE)
}

fn store_config(config: EarlyUartConfig, source: EarlyConsoleSource) {
    UART_PHYS_BASE.store(config.phys_base, Ordering::Relaxed);
    UART_CLOCK_HZ.store(config.clock_hz as usize, Ordering::Relaxed);
    UART_BAUD.store(config.baud as usize, Ordering::Relaxed);
    UART_REG_OFFSET.store(config.reg_offset, Ordering::Relaxed);
    UART_REG_SHIFT.store(config.reg_shift as usize, Ordering::Relaxed);
    UART_IO_WIDTH.store(config.io_width.bytes(), Ordering::Relaxed);
    UART_ENDIAN.store(config.endian as usize, Ordering::Relaxed);
    UART_SOURCE.store(source as usize, Ordering::Release);
}

fn load_source() -> EarlyConsoleSource {
    match UART_SOURCE.load(Ordering::Acquire) {
        value if value == EarlyConsoleSource::DeviceTree as usize => EarlyConsoleSource::DeviceTree,
        _ => EarlyConsoleSource::Fallback,
    }
}

fn load_config() -> EarlyUartConfig {
    // 与 `store_config` 最后的 Release 发布配对，确保其它 CPU 不会观察到混合配置。
    let _ = UART_SOURCE.load(Ordering::Acquire);
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
    DMW0_UNCACHED_BASE
        | config
            .phys_base
            .checked_add(offset)
            .expect("validated early UART register address")
}

fn write_register(config: EarlyUartConfig, register: usize, value: u8) {
    let address = register_address(config, register);
    // Safety: 配置在发布前已验证 MMIO 基址、寄存器步长和访问宽度；DMW0 被启动
    // 汇编配置为 uncached，且这里只执行 binding 允许的对齐易失访问。
    unsafe {
        match config.io_width {
            RegisterIoWidth::U8 => core::ptr::write_volatile(address as *mut u8, value),
            RegisterIoWidth::U16 => {
                let value = match config.endian {
                    RegisterEndian::Little => u16::from(value).to_le(),
                    RegisterEndian::Big => u16::from(value).to_be(),
                };
                core::ptr::write_volatile(address as *mut u16, value)
            }
            RegisterIoWidth::U32 => {
                let value = match config.endian {
                    RegisterEndian::Little => u32::from(value).to_le(),
                    RegisterEndian::Big => u32::from(value).to_be(),
                };
                core::ptr::write_volatile(address as *mut u32, value)
            }
        }
    }
}

fn read_register(config: EarlyUartConfig, register: usize) -> u8 {
    let address = register_address(config, register);
    // Safety: 与 `write_register` 相同；读取值只使用 16550 寄存器的低 8 位。
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

fn ensure_uart_initialized(config: EarlyUartConfig) {
    match UART_STATE.compare_exchange(
        UART_STATE_UNINITIALIZED,
        UART_STATE_INITIALIZING,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {
            let divisor = config
                .divisor()
                .expect("published early UART configuration must have a valid divisor");
            write_register(config, REG_IER, 0);
            write_register(config, REG_LCR, UART_LCR_DLAB);
            write_register(config, REG_THR, divisor as u8);
            write_register(config, REG_IER, (divisor >> 8) as u8);
            write_register(config, REG_LCR, UART_LCR_8N1);
            write_register(config, REG_FCR, UART_FCR_ENABLE_FIFO | UART_FCR_CLEAR_RXTX);
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
    ensure_uart_initialized(config);

    let mut offset = 0usize;
    while offset < bytes.len() {
        while read_register(config, REG_LSR) & UART_LSR_THRE == 0 {
            core::hint::spin_loop();
        }
        let count = (bytes.len() - offset).min(UART_FIFO_DEPTH);
        for &byte in &bytes[offset..offset + count] {
            write_register(config, REG_THR, byte);
        }
        offset += count;
    }
}

struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, value: &str) -> core::fmt::Result {
        console_write_bytes(value.as_bytes());
        Ok(())
    }
}

pub fn e_print(args: core::fmt::Arguments) {
    ConsoleWriter.write_fmt(args).unwrap();
}

pub fn e_write_bytes(bytes: &[u8]) {
    console_write_bytes(bytes);
}
