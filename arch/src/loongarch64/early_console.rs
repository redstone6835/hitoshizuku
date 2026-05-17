//! LoongArch64 早期串口输出。
//!
//! 这个模块提供“正式设备模型和 logger 尚未建立之前”的最小输出能力，目的是让
//! 启动路径上的关键阶段能够尽早打印诊断信息。它直接通过 DMW0 uncached 窗口访问
//! UART MMIO 寄存器，不依赖：
//!
//! - 动态内存分配；
//! - 正式页表映射；
//! - 设备注册框架；
//! - 上层 console 抽象。
//!
//! 因为它运行得非常早，所以实现风格刻意保持简单直接：常量地址、轮询发送、一次性
//! 初始化。等 `init` 阶段注册好正式串口和 logger 之后，这条路径就退居为启动期
//! 兜底通道。
use core::fmt::Write;

fn console_write_bytes(bytes: &[u8]) {
    const UART_BASE: usize = 0x1fe0_01e0;
    const DMW0_UNCACHED_BASE: usize = 0x8000_0000_0000_0000;
    const UART_THR: *mut u8 = (DMW0_UNCACHED_BASE | UART_BASE) as *mut u8;
    const UART_IER: *mut u8 = (DMW0_UNCACHED_BASE | (UART_BASE + 1)) as *mut u8;
    const UART_FCR: *mut u8 = (DMW0_UNCACHED_BASE | (UART_BASE + 2)) as *mut u8;
    const UART_LCR: *mut u8 = (DMW0_UNCACHED_BASE | (UART_BASE + 3)) as *mut u8;
    const UART_MCR: *mut u8 = (DMW0_UNCACHED_BASE | (UART_BASE + 4)) as *mut u8;
    const UART_LSR: *mut u8 = (DMW0_UNCACHED_BASE | (UART_BASE + 5)) as *mut u8;
    const UART_LSR_THRE: u8 = 1 << 5;
    const UART_LCR_DLAB: u8 = 1 << 7;
    const UART_LCR_8N1: u8 = 0x03;
    const UART_FCR_ENABLE_FIFO: u8 = 0x01;
    const UART_FCR_CLEAR_RXTX: u8 = 0x06;
    const UART_MCR_DTR_RTS: u8 = 0x03;
    const UART_FIFO_DEPTH: usize = 16;
    const UART_DLL_115200_AT_100MHZ: u8 = 54;
    const UART_DLM_115200_AT_100MHZ: u8 = 0;

    static mut UART_INITIALIZED: bool = false;

    #[allow(deprecated)]
    unsafe {
        if !UART_INITIALIZED {
            core::ptr::write_volatile(UART_IER, 0x00);
            core::ptr::write_volatile(UART_LCR, UART_LCR_DLAB);
            core::ptr::write_volatile(UART_THR, UART_DLL_115200_AT_100MHZ);
            core::ptr::write_volatile(UART_IER, UART_DLM_115200_AT_100MHZ);
            core::ptr::write_volatile(UART_LCR, UART_LCR_8N1);
            core::ptr::write_volatile(UART_FCR, UART_FCR_ENABLE_FIFO | UART_FCR_CLEAR_RXTX);
            core::ptr::write_volatile(UART_MCR, UART_MCR_DTR_RTS);
            UART_INITIALIZED = true;
        }

        let mut offset = 0usize;
        while offset < bytes.len() {
            while core::ptr::read_volatile(UART_LSR) & UART_LSR_THRE == 0 {}
            let count = (bytes.len() - offset).min(UART_FIFO_DEPTH);
            for &byte in &bytes[offset..offset + count] {
                core::ptr::write_volatile(UART_THR, byte);
            }
            offset += count;
        }
    }
}

struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        console_write_bytes(s.as_bytes());
        Ok(())
    }
}

pub fn e_print(args: core::fmt::Arguments) {
    ConsoleWriter.write_fmt(args).unwrap();
}

pub fn e_write_bytes(bytes: &[u8]) {
    console_write_bytes(bytes);
}
