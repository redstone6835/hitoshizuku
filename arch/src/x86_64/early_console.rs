//! x86_64 早期串口输出。
//!
//! 在 ACPI/驱动模型和正式 console 建立之前，PC 兼容固件通常仍保留
//! 16550A COM1（I/O 端口 `0x3f8`）。这里提供一个有界的 polled 输出路径：
//! 裸机目标写真实端口，hosted 构建完全不触碰 I/O 指令，因而可以安全地做
//! 启动协议和日志单测。正式 console 建立后仍可继续使用它作为故障回退出口。

use core::sync::atomic::{AtomicU8, AtomicU16, Ordering};

use super::io::{inb, outb};

/// 8250/16550 数据寄存器相对基址偏移。
const REG_THR: u16 = 0;
const REG_IER: u16 = 1;
const REG_LCR: u16 = 3;
const REG_FCR: u16 = 2;
const REG_MCR: u16 = 4;
const REG_LSR: u16 = 5;

const LCR_DLAB: u8 = 1 << 7;
const LCR_8N1: u8 = 0x03;
const FCR_ENABLE_FIFO: u8 = 1 << 0;
const FCR_CLEAR_RX: u8 = 1 << 1;
const FCR_CLEAR_TX: u8 = 1 << 2;
const MCR_DTR: u8 = 1 << 0;
const MCR_RTS: u8 = 1 << 1;
const MCR_OUT2: u8 = 1 << 3;
const LSR_THRE: u8 = 1 << 5;

const UART_UNINITIALIZED: u8 = 0;
const UART_INITIALIZING: u8 = 1;
const UART_READY: u8 = 2;
const UART_WAIT_LIMIT: usize = 1 << 16;

static UART_STATE: AtomicU8 = AtomicU8::new(UART_UNINITIALIZED);
static UART_BASE: AtomicU16 = AtomicU16::new(0x3f8);

/// 选择早期 16550 端口。调用者应在并发输出开始前调用；传入零恢复 COM1。
///
/// ACPI SPCR/PNP0C02 解析得到的 SystemIO 串口可以通过此入口覆盖默认值。
pub fn set_port(base: u16) {
    UART_BASE.store(if base == 0 { 0x3f8 } else { base }, Ordering::Release);
    UART_STATE.store(UART_UNINITIALIZED, Ordering::Release);
}

/// 返回当前早期串口基址。
pub fn port() -> u16 {
    UART_BASE.load(Ordering::Acquire)
}

#[inline]
fn register(base: u16, offset: u16) -> u16 {
    base.saturating_add(offset)
}

fn initialize() {
    let base = port();
    // Disable UART interrupts: the early path is polled and has no IDT handler
    // yet.  Divisor 1 selects 115200 baud for the conventional 1.8432 MHz clock.
    unsafe {
        outb(register(base, REG_IER), 0);
        outb(register(base, REG_LCR), LCR_DLAB);
        outb(register(base, REG_THR), 1);
        outb(register(base, REG_IER), 0);
        outb(register(base, REG_LCR), LCR_8N1);
        outb(
            register(base, REG_FCR),
            FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX,
        );
        outb(register(base, REG_MCR), MCR_DTR | MCR_RTS | MCR_OUT2);
    }
}

fn ensure_initialized() -> u16 {
    let state = UART_STATE.load(Ordering::Acquire);
    if state == UART_READY {
        return port();
    }
    if UART_STATE
        .compare_exchange(
            UART_UNINITIALIZED,
            UART_INITIALIZING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        initialize();
        UART_STATE.store(UART_READY, Ordering::Release);
    } else {
        // A nested early log can observe INITIALIZING while the owner is doing
        // port setup.  Bounded waiting avoids an unrecoverable deadlock if the
        // owner faults before publishing READY.
        for _ in 0..UART_WAIT_LIMIT {
            if UART_STATE.load(Ordering::Acquire) == UART_READY {
                break;
            }
            core::hint::spin_loop();
        }
    }
    port()
}

/// 输出一个字节。UART 不响应时在有限轮询后丢弃该字节，不能阻塞启动。
pub fn write_byte(byte: u8) {
    let base = ensure_initialized();
    for _ in 0..UART_WAIT_LIMIT {
        let ready = unsafe { inb(register(base, REG_LSR)) } & LSR_THRE != 0;
        if ready {
            unsafe { outb(register(base, REG_THR), byte) };
            return;
        }
        core::hint::spin_loop();
    }
}

/// 输出字节串；保持调用方字节不变，仅在 LF 前补 CR 以兼容传统终端。
pub fn write_bytes(bytes: &[u8]) {
    for &byte in bytes {
        if byte == b'\n' {
            write_byte(b'\r');
        }
        write_byte(byte);
    }
}

/// 输出固定宽度的 16 个十六进制字符（不带 `0x` 前缀）。
pub fn write_hex16(value: usize) {
    let digits = b"0123456789abcdef";
    let width = core::mem::size_of::<usize>() * 2;
    for index in (0..width).rev() {
        let nibble = ((value >> (index * 4)) & 0xf) as usize;
        write_byte(digits[nibble]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port_and_override_are_stable() {
        set_port(0);
        assert_eq!(port(), 0x3f8);
        set_port(0x2f8);
        assert_eq!(port(), 0x2f8);
        set_port(0);
    }

    #[test]
    fn hosted_output_does_not_panic() {
        set_port(0);
        write_bytes(b"x86\n");
        write_hex16(0x1234);
    }
}
