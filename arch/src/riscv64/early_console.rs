//! RISC-V64 早期串口输出。
//!
//! 在正式设备模型和 logger 尚未建立之前提供最小输出能力，使启动路径上的关键阶段
//! 能够尽早打印诊断信息。直接轮询 NS16550 UART MMIO 寄存器，不依赖内存分配、
//! 正式页表、设备注册框架或上层 console 抽象。
//!
//! 启动初期通过 identity mapping 以物理地址 `0x1000_0000` 访问 UART。
//! 内核页表初始化后需调用 [`switch_to_virtual`] 切换到 MMIO 虚拟地址，
//! 否则 identity mapping 被拆除后输出将不可用。
//!
//! ```text
//! NS16550A 寄存器（byte 宽度，基地址 + offset）：
//!
//!   Offset | DLAB=0 读      | DLAB=0 写      | DLAB=1
//!   ───────┼────────────────┼────────────────┼───────────
//!     0    | RBR (接收)     | THR (发送)     | DLL (除数低)
//!     1    | IER (中断使能) | IER            | DLM (除数高)
//!     2    | IIR (中断 ID)  | FCR (FIFO 控制)|
//!     3    | LCR (线路控制) | LCR            |
//!     4    | MCR (Modem)    | MCR            |
//!     5    | LSR (线路状态) | —              |
//! ```

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ── NS16550A 常量 ─────────────────────────────────────────────────────────────

/// UART 物理地址（QEMU virt 平台 NS16550A）。
const UART_PHYS: usize = 0x1000_0000;

// NS16550 寄存器偏移
const THR: usize = 0; // Transmit Holding Register
const IER: usize = 1; // Interrupt Enable Register
const FCR: usize = 2; // FIFO Control Register
const LCR: usize = 3; // Line Control Register
const MCR: usize = 4; // Modem Control Register
const LSR: usize = 5; // Line Status Register

const LSR_THRE: u8 = 1 << 5; // TX holding register empty
const LCR_DLAB: u8 = 1 << 7; // Divisor Latch Access Bit
const LCR_8N1: u8 = 0x03; // 8 data bits, no parity, 1 stop
const FCR_ENABLE_CLEAR: u8 = 0x07; // FIFO enable + clear RX/TX
const MCR_DTR_RTS: u8 = 0x03; // DTR + RTS asserted

// 波特率 115200 @ 1.8432 MHz 参考时钟（QEMU 不关心实际值）
const DLL: u8 = 1;
const DLM: u8 = 0;

// ── 运行时状态 ────────────────────────────────────────────────────────────────

/// 运行时 UART 基地址，启动后可通过 [`switch_to_virtual`] 更新。
static BASE: AtomicUsize = AtomicUsize::new(UART_PHYS);

/// 是否已完成 16550 硬件初始化。
static INITED: AtomicBool = AtomicBool::new(false);

// ── 公开 API ──────────────────────────────────────────────────────────────────

/// 内核页表就绪后调用，将 UART 访问切换到 MMIO 虚拟地址。
///
/// # 调用时序
///
/// 必须在以下条件同时满足时调用：
/// - MMIO 虚拟映射已建立（新地址可达）
/// - identity mapping 尚未拆除（旧地址仍可达，确保切换窗口安全）
pub fn switch_to_virtual() {
    let vaddr = UART_PHYS.wrapping_add(crate::riscv64::heap_vm::MMIO_VIRT_BASE);
    BASE.store(vaddr, Ordering::Release);
}

/// 格式化输出到早期串口。
pub fn e_print(args: core::fmt::Arguments) {
    use core::fmt::Write;
    struct W;
    impl core::fmt::Write for W {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            write_bytes(s.as_bytes());
            Ok(())
        }
    }
    let _ = W.write_fmt(args);
}

/// 原始字节输出到早期串口。
pub fn e_write_bytes(bytes: &[u8]) {
    write_bytes(bytes);
}

// ── 内部实现 ──────────────────────────────────────────────────────────────────

/// 底层发送：首次调用初始化硬件，之后逐字节轮询写入。
///
/// 每字节写入前等待 LSR.THRE=1（TX holding register 空），保证不丢数据。
/// 不做 FIFO 批量优化——早期输出量小，正确性优先。
///
/// 防重入：若初始化过程中被中断且中断 handler 也调 e_print，跳过初始化直接写。
fn write_bytes(bytes: &[u8]) {
    use core::sync::atomic::AtomicBool;
    static INITIALIZING: AtomicBool = AtomicBool::new(false);

    let base = BASE.load(Ordering::Acquire);

    unsafe {
        if !INITED.load(Ordering::Acquire) {
            // 防重入：如果已经在初始化中（被中断嵌套调用），跳过初始化直接输出
            if INITIALIZING
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                // 16550 标准初始化序列
                reg(base, IER).write_volatile(0x00); // 关闭所有中断
                reg(base, LCR).write_volatile(LCR_DLAB); // 开 DLAB 设波特率
                reg(base, THR).write_volatile(DLL); // Divisor Latch Low
                reg(base, IER).write_volatile(DLM); // Divisor Latch High
                reg(base, LCR).write_volatile(LCR_8N1); // 8N1, 关 DLAB
                reg(base, FCR).write_volatile(FCR_ENABLE_CLEAR); // 使能并清空 FIFO
                reg(base, MCR).write_volatile(MCR_DTR_RTS); // 拉高 DTR/RTS
                INITED.store(true, Ordering::Release);
                INITIALIZING.store(false, Ordering::Release);
            }
            // else: 重入调用，跳过初始化，直接写（QEMU 上 UART 未初始化也能写 THR）
        }

        let thr = reg(base, THR);
        let lsr = reg(base, LSR) as *const u8;
        for &b in bytes {
            while lsr.read_volatile() & LSR_THRE == 0 {}
            thr.write_volatile(b);
        }
    }
}

#[inline(always)]
fn reg(base: usize, offset: usize) -> *mut u8 {
    (base + offset) as *mut u8
}
