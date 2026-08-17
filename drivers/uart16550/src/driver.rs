//! NS16550A 兼容 UART ELM 驱动程序。
//!
//! 模块内包含两层能力：底层 [`Uart16550`] 负责直接访问寄存器；PnP 适配层
//! [`Uart16550PlatformDriver`] 负责匹配固件枚举的 platform 串口并注册字符
//! function。内建注册入口只提交 factory，不参与固件扫描。

use alloc::sync::Arc;
use core::any::Any;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use sched::{Task, WaitQueue};

use crate::dev::char::*;
use crate::dev::dt_provider::{
    self, DtbProviderError, DtbResourceLease, DtbResourceReply, DtbResourceRequest,
};
use crate::dev::function::{CharFunction, FunctionProjectionNameAllocator};
use crate::dev::irq::{self, IrqError, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use crate::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpDependency, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

// ─────────────────────── UART 寄存器偏移与标志 ───────────────────────────

const REG_THR: usize = 0; // Transmit Holding Register (write, DLAB=0)
const REG_RBR: usize = 0; // Receive Buffer Register  (read,  DLAB=0)
const REG_DLL: usize = 0; // Divisor Latch Low        (write, DLAB=1)
const REG_IER: usize = 1; // Interrupt Enable / DLM   (DLAB=1 时为高字节除数)
const REG_FCR: usize = 2; // FIFO Control Register    (write)
const REG_LCR: usize = 3; // Line Control Register
const REG_MCR: usize = 4; // Modem Control Register
const REG_LSR: usize = 5; // Line Status Register

const LSR_DR: u8 = 1 << 0; // Data Ready（接收缓冲非空）
const LSR_THRE: u8 = 1 << 5; // TX Holding Register Empty（发送 FIFO 空）
const LSR_TEMT: u8 = 1 << 6; // Transmitter Empty（FIFO + 移位寄存器均空）
const IER_RDI: u8 = 1 << 0; // Received Data Available Interrupt
const LCR_DLAB: u8 = 1 << 7; // Divisor Latch Access Bit
const LCR_BREAK: u8 = 1 << 6; // Break Control（发送线保持空号条件）
const LCR_8N1: u8 = 0x03; // 8 数据位，无奇偶校验，1 停止位
const FCR_ENABLE_FIFO: u8 = 0x01;
const FCR_CLEAR_RX: u8 = 0x02;
const FCR_CLEAR_TX: u8 = 0x04;
const FCR_CLEAR_RXTX: u8 = FCR_CLEAR_RX | FCR_CLEAR_TX;
const MCR_DTR_RTS: u8 = 0x03;

/// 16550 divisor 公式中的固定过采样倍率。
const UART_DIVISOR_OVERSAMPLE: u32 = 16;
/// 固件未提供 baud 属性时采用的传统串口默认波特率。
const UART_DEFAULT_BAUD: u32 = 115_200;
/// 标准 16550 FIFO 深度。
const FIFO_DEPTH: usize = 16;
/// 保守按 THR ready 单字节发送，避免把一次状态快照当成 FIFO 可用容量。
const TX_FIFO_BATCH: usize = 1;
/// 软件发送缓冲区大小。
const TX_SW_BUFFER_SIZE: usize = 32 * 1024;
/// flush/write_all 等待硬件发送完成时的自旋上限。
const TX_SPIN_RETRY_LIMIT: usize = 10_000_000;
/// break 保持时间换算。
const NS_PER_MS: u64 = 1_000_000;

const UART_CLOCK_PROPERTY: &str = "clocks";
const UART_BAUD_CLOCK_NAME: &str = "baud";

fn uart_divisor(clock_hz: u32, baud: u32) -> Option<u16> {
    let denominator = baud.checked_mul(UART_DIVISOR_OVERSAMPLE)?;
    let divisor = clock_hz.checked_div(denominator)?;
    (divisor != 0 && divisor <= u32::from(u16::MAX)).then_some(divisor as u16)
}

fn uart_provider_clock(
    info: &PlatformDeviceInfo,
) -> Result<Option<(u32, DtbResourceLease)>, PnpError> {
    let reference = info
        .dtb_reference_by_name(UART_CLOCK_PROPERTY, UART_BAUD_CLOCK_NAME)
        .or_else(|| info.dtb_references(UART_CLOCK_PROPERTY).next());
    let Some(reference) = reference else {
        return Ok(None);
    };
    let lease =
        dt_provider::acquire_reference(reference).map_err(DtbProviderError::into_pnp_error)?;
    let rate = match lease
        .control(DtbResourceRequest::GetRate)
        .map_err(DtbProviderError::into_pnp_error)?
    {
        DtbResourceReply::Value(rate) => rate,
        _ => {
            return Err(PnpError::malformed(
                PnpResourceKind::Other("clock"),
                "uart clock provider returned a non-rate reply",
            ));
        }
    };
    let rate = u32::try_from(rate).map_err(|_| {
        PnpError::malformed(
            PnpResourceKind::Other("clock"),
            "uart clock rate does not fit the binding's 32-bit frequency",
        )
    })?;
    if rate == 0 {
        return Err(PnpError::malformed(
            PnpResourceKind::Other("clock"),
            "uart clock provider returned zero Hz",
        ));
    }
    Ok(Some((rate, lease)))
}

struct UartTxState {
    buf: [u8; TX_SW_BUFFER_SIZE],
    head: usize,
    tail: usize,
    len: usize,
}

impl UartTxState {
    const fn new() -> Self {
        Self {
            buf: [0; TX_SW_BUFFER_SIZE],
            head: 0,
            tail: 0,
            len: 0,
        }
    }
}

struct UartTxBuffer {
    state: UnsafeCell<UartTxState>,
    lock: AtomicUsize,
}

// Safety: `state` 的共享可变访问只会在成功持有 `lock` 后发生，守卫析构时以
// Release 顺序释放锁；因此不同执行上下文不会同时取得可变引用。
unsafe impl Sync for UartTxBuffer {}

impl UartTxBuffer {
    const fn new() -> Self {
        Self {
            state: UnsafeCell::new(UartTxState::new()),
            lock: AtomicUsize::new(0),
        }
    }

    fn try_lock(&self) -> Option<UartTxGuard<'_>> {
        self.lock
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(UartTxGuard { buffer: self })
    }

    fn lock(&self) -> UartTxGuard<'_> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            core::hint::spin_loop();
        }
    }
}

struct UartTxGuard<'a> {
    buffer: &'a UartTxBuffer,
}

impl UartTxGuard<'_> {
    fn state_mut(&mut self) -> &mut UartTxState {
        // Safety: 当前守卫独占对应的自旋锁，且返回引用的生命周期不超过守卫。
        unsafe { &mut *self.buffer.state.get() }
    }
}

impl Drop for UartTxGuard<'_> {
    fn drop(&mut self) {
        self.buffer.lock.store(0, Ordering::Release);
    }
}

/// 接收侧临界区守卫。见 [`Uart16550::rx_lock`]。
struct UartRxGuard<'a> {
    lock: &'a AtomicUsize,
}

impl Drop for UartRxGuard<'_> {
    fn drop(&mut self) {
        self.lock.store(0, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UartRegisterWidth {
    U8,
    U16,
    U32,
}

impl UartRegisterWidth {
    const fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }

    const fn from_bytes(bytes: u32) -> Option<Self> {
        match bytes {
            1 => Some(Self::U8),
            2 => Some(Self::U16),
            4 => Some(Self::U32),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UartRegisterEndian {
    Little,
    Big,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UartRegisterConfigError {
    InvalidProperty,
    ConflictingEndian,
    InvalidShift,
    InvalidWidth,
    Misaligned,
    WindowTooSmall,
    AddressOverflow,
}

/// 已按 8250 DT binding 校验的寄存器访问布局。
#[derive(Clone, Copy)]
struct UartRegisterAccess {
    base: usize,
    reg_offset: usize,
    reg_shift: u32,
    width: UartRegisterWidth,
    endian: UartRegisterEndian,
}

impl UartRegisterAccess {
    fn from_platform(
        info: &PlatformDeviceInfo,
        base: usize,
        window_size: usize,
    ) -> Result<Self, UartRegisterConfigError> {
        let reg_offset = optional_u32_property(info, "reg-offset")?.unwrap_or(0) as usize;
        let reg_shift = optional_u32_property(info, "reg-shift")?.unwrap_or(0);
        if reg_shift >= usize::BITS {
            return Err(UartRegisterConfigError::InvalidShift);
        }
        let width = UartRegisterWidth::from_bytes(
            optional_u32_property(info, "reg-io-width")?.unwrap_or(1),
        )
        .ok_or(UartRegisterConfigError::InvalidWidth)?;
        let endian = uart_register_endian(info)?;
        let stride = 1usize
            .checked_shl(reg_shift)
            .ok_or(UartRegisterConfigError::AddressOverflow)?;
        if stride < width.bytes() {
            return Err(UartRegisterConfigError::Misaligned);
        }
        let first = base
            .checked_add(reg_offset)
            .ok_or(UartRegisterConfigError::AddressOverflow)?;
        if !first.is_multiple_of(width.bytes()) {
            return Err(UartRegisterConfigError::Misaligned);
        }
        let last_offset = REG_LSR
            .checked_mul(stride)
            .and_then(|offset| reg_offset.checked_add(offset))
            .ok_or(UartRegisterConfigError::AddressOverflow)?;
        let span = last_offset
            .checked_add(width.bytes())
            .ok_or(UartRegisterConfigError::AddressOverflow)?;
        base.checked_add(span)
            .ok_or(UartRegisterConfigError::AddressOverflow)?;
        if window_size != 0 && span > window_size {
            return Err(UartRegisterConfigError::WindowTooSmall);
        }
        Ok(Self {
            base,
            reg_offset,
            reg_shift,
            width,
            endian,
        })
    }

    fn address(self, register: usize) -> usize {
        let offset = register
            .checked_shl(self.reg_shift)
            .and_then(|offset| self.reg_offset.checked_add(offset))
            .expect("validated UART register layout must remain representable");
        self.base
            .checked_add(offset)
            .expect("validated UART register address must remain representable")
    }

    fn read(self, register: usize) -> u8 {
        let address = self.address(register);
        // Safety: probe 已按 DT reg 窗口、reg-offset、reg-shift、访问宽度和对齐完整
        // 校验该地址；这里只执行对应宽度的易失 MMIO 读取并取 16550 低 8 位。
        unsafe {
            match self.width {
                UartRegisterWidth::U8 => core::ptr::read_volatile(address as *const u8),
                UartRegisterWidth::U16 => {
                    let raw = core::ptr::read_volatile(address as *const u16);
                    match self.endian {
                        UartRegisterEndian::Little => u16::from_le(raw) as u8,
                        UartRegisterEndian::Big => u16::from_be(raw) as u8,
                    }
                }
                UartRegisterWidth::U32 => {
                    let raw = core::ptr::read_volatile(address as *const u32);
                    match self.endian {
                        UartRegisterEndian::Little => u32::from_le(raw) as u8,
                        UartRegisterEndian::Big => u32::from_be(raw) as u8,
                    }
                }
            }
        }
    }

    fn write(self, register: usize, value: u8) {
        let address = self.address(register);
        // Safety: 安全条件与 `read` 相同；目标是 16550 寄存器，写值按 binding
        // 声明的 MMIO 宽度与端序扩展。
        unsafe {
            match self.width {
                UartRegisterWidth::U8 => core::ptr::write_volatile(address as *mut u8, value),
                UartRegisterWidth::U16 => {
                    let value = match self.endian {
                        UartRegisterEndian::Little => u16::from(value).to_le(),
                        UartRegisterEndian::Big => u16::from(value).to_be(),
                    };
                    core::ptr::write_volatile(address as *mut u16, value);
                }
                UartRegisterWidth::U32 => {
                    let value = match self.endian {
                        UartRegisterEndian::Little => u32::from(value).to_le(),
                        UartRegisterEndian::Big => u32::from(value).to_be(),
                    };
                    core::ptr::write_volatile(address as *mut u32, value);
                }
            }
        }
    }
}

fn optional_u32_property(
    info: &PlatformDeviceInfo,
    name: &str,
) -> Result<Option<u32>, UartRegisterConfigError> {
    let Some(raw) = info.bytes_property(name) else {
        return Ok(None);
    };
    let bytes: [u8; 4] = raw
        .try_into()
        .map_err(|_| UartRegisterConfigError::InvalidProperty)?;
    Ok(Some(u32::from_be_bytes(bytes)))
}

fn strict_bool_property(
    info: &PlatformDeviceInfo,
    name: &str,
) -> Result<bool, UartRegisterConfigError> {
    match info.bytes_property(name) {
        None => Ok(false),
        Some([]) => Ok(true),
        Some(_) => Err(UartRegisterConfigError::InvalidProperty),
    }
}

fn uart_register_endian(
    info: &PlatformDeviceInfo,
) -> Result<UartRegisterEndian, UartRegisterConfigError> {
    let big = strict_bool_property(info, "big-endian")?;
    let little = strict_bool_property(info, "little-endian")?;
    let native = strict_bool_property(info, "native-endian")?;
    if usize::from(big) + usize::from(little) + usize::from(native) > 1 {
        return Err(UartRegisterConfigError::ConflictingEndian);
    }
    if big {
        Ok(UartRegisterEndian::Big)
    } else if native && cfg!(target_endian = "big") {
        Ok(UartRegisterEndian::Big)
    } else {
        Ok(UartRegisterEndian::Little)
    }
}

// ─────────────────────────── Uart16550 ────────────────────────────────────

/// NS16550A 兼容 UART 驱动（内存映射 I/O）。
pub struct Uart16550 {
    /// UART 寄存器组的已校验访问布局。
    registers: UartRegisterAccess,
    /// UART 输入时钟；固件预配置路径可能没有稳定来源。
    clock_hz: Option<u32>,
    /// 软件发送缓冲区。日志路径先入队，再由轮询/flush 持续排空到 UART FIFO。
    tx: UartTxBuffer,
    /// RX 等待队列。硬件中断只负责唤醒，真正读取和行规程仍在 VFS/TTY 路径完成。
    rx_wait: WaitQueue,
    /// 接收侧串行化锁。
    ///
    /// 读 RBR 是**破坏性**操作：一次读就把字节从硬件 FIFO 弹出。多核下若两个
    /// 读者同时看到 LSR.DR=1 并各自读 RBR，同一串输入会被拆散到两个调用者
    /// 手里，且顺序不可控——串口上就表现为字符换位（例如 `/tmp/p/run.sh`
    /// 变成 `/tm/pp/run.sh`），进而让宿主侧发下来的命令执行失败。
    rx_lock: AtomicUsize,
}

// Safety: MMIO 寄存器允许跨执行上下文访问，软件发送状态由 `UartTxBuffer` 串行化，
// 等待队列自身提供并发保护，实例不含依赖线程亲和性的引用。
unsafe impl Send for Uart16550 {}
// Safety: 共享状态的并发约束与上面的 `Send` 实现相同。
unsafe impl Sync for Uart16550 {}

impl Uart16550 {
    /// 创建并初始化一个 UART 驱动实例。
    ///
    /// - `virt_base`: UART 寄存器的基地址。
    /// - `clock_hz`: UART 输入时钟频率（Hz）。
    /// - `baud`: 目标波特率（如 `115_200`）。
    fn new(registers: UartRegisterAccess, clock_hz: u32, baud: u32) -> Result<Self, UartError> {
        let divisor = uart_divisor(clock_hz, baud).ok_or(UartError::InvalidBaudRate)?;
        let uart = Self {
            registers,
            clock_hz: Some(clock_hz),
            tx: UartTxBuffer::new(),
            rx_wait: WaitQueue::new(),
            rx_lock: AtomicUsize::new(0),
        };
        uart.init(divisor);
        Ok(uart)
    }

    /// 接管固件已经初始化过的 UART，不重新编程 divisor。
    ///
    /// ACPI SPCR 允许固件描述一个已经用于 console redirection 的串口，但旧版
    /// SPCR 不一定提供 UART 输入时钟。此路径只关闭中断并启用 FIFO/握手，避免
    /// 引入平台默认时钟 fallback。
    fn new_preconfigured(registers: UartRegisterAccess) -> Self {
        let uart = Self {
            registers,
            clock_hz: None,
            tx: UartTxBuffer::new(),
            rx_wait: WaitQueue::new(),
            rx_lock: AtomicUsize::new(0),
        };
        uart.attach_preconfigured();
        uart
    }

    /// 返回 UART 寄存器组的虚拟基地址。
    #[inline]
    pub fn base(&self) -> usize {
        self.registers.base
    }

    #[inline]
    fn read_reg(&self, offset: usize) -> u8 {
        self.registers.read(offset)
    }

    #[inline]
    fn write_reg(&self, offset: usize, value: u8) {
        self.registers.write(offset, value)
    }

    #[inline]
    fn line_status(&self) -> u8 {
        self.read_reg(REG_LSR)
    }

    /// 取得接收侧独占权。
    ///
    /// "检查 LSR.DR 再读 RBR"必须是原子的，否则两个读者会各自弹走一部分字节，
    /// 造成输入乱序。中断上下文也可能进入这里，因此用自旋而不是睡眠锁；临界区
    /// 只有几次 MMIO 访问，非常短。
    fn lock_rx(&self) -> UartRxGuard<'_> {
        while self
            .rx_lock
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            core::hint::spin_loop();
        }
        UartRxGuard {
            lock: &self.rx_lock,
        }
    }

    #[inline]
    fn transmitter_fifo_ready(&self) -> bool {
        self.line_status() & LSR_THRE != 0
    }

    #[inline]
    fn transmitter_empty(&self) -> bool {
        self.line_status() & LSR_TEMT != 0
    }

    fn init(&self, divisor: u16) {
        let dll = (divisor & 0xff) as u8;
        let dlm = (divisor >> 8) as u8;

        self.write_reg(REG_IER, 0x00);
        self.write_reg(REG_LCR, LCR_DLAB);
        self.write_reg(REG_DLL, dll);
        self.write_reg(REG_IER, dlm);
        self.write_reg(REG_LCR, LCR_8N1);
        self.write_reg(REG_FCR, FCR_ENABLE_FIFO | FCR_CLEAR_RXTX);
        self.write_reg(REG_MCR, MCR_DTR_RTS);
    }

    fn attach_preconfigured(&self) {
        // 预配置串口通常已经被固件接到 QEMU 标准输入输出或控制台。保持接收中断
        // 使能，避免后端因客户机未声明可接收而停止投递输入；实际消费仍走轮询读取。
        self.write_reg(REG_IER, IER_RDI);
        self.write_reg(REG_FCR, FCR_ENABLE_FIFO);
        self.write_reg(REG_MCR, MCR_DTR_RTS);
    }

    fn set_rx_irq_enabled(&self, enabled: bool) {
        let value = if enabled { IER_RDI } else { 0 };
        // 本驱动只使用接收就绪中断；关闭时直接写 0，避免保留未知固件残留位。
        self.write_reg(REG_IER, value);
    }

    fn wake_rx_waiters_if_ready(&self) -> IrqStatus {
        if self.poll_read() {
            self.rx_wait.wake_all();
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }

    fn queued_output_len(&self) -> u32 {
        self.tx.lock().state_mut().len.min(u32::MAX as usize) as u32
    }

    fn discard_rx_fifo(&self) {
        let _guard = self.lock_rx();
        self.write_reg(REG_FCR, FCR_ENABLE_FIFO | FCR_CLEAR_RX);
        // FCR 清空后再读掉残留的 Data Ready，覆盖实现存在延迟的 UART 变体。
        for _ in 0..FIFO_DEPTH {
            if self.line_status() & LSR_DR == 0 {
                break;
            }
            let _ = self.read_reg(REG_RBR);
        }
    }

    fn discard_tx_fifo_and_buffer(&self) {
        let mut guard = self.tx.lock();
        let state = guard.state_mut();
        state.head = 0;
        state.tail = 0;
        state.len = 0;
        self.write_reg(REG_FCR, FCR_ENABLE_FIFO | FCR_CLEAR_TX);
    }

    fn discard_rx_tx(&self) {
        {
            let mut guard = self.tx.lock();
            let state = guard.state_mut();
            state.head = 0;
            state.tail = 0;
            state.len = 0;
            self.write_reg(REG_FCR, FCR_ENABLE_FIFO | FCR_CLEAR_RXTX);
        }
        let _guard = self.lock_rx();
        for _ in 0..FIFO_DEPTH {
            if self.line_status() & LSR_DR == 0 {
                break;
            }
            let _ = self.read_reg(REG_RBR);
        }
    }

    fn set_break_condition(&self, enabled: bool) {
        let mut lcr = self.read_reg(REG_LCR);
        if enabled {
            lcr |= LCR_BREAK;
        } else {
            lcr &= !LCR_BREAK;
        }
        self.write_reg(REG_LCR, lcr);
    }

    fn wait_break_duration(&self, duration_ms: u32) -> Result<(), ControlError> {
        if duration_ms == 0 {
            return Ok(());
        }
        let start = sched::now_ns_public();
        if start == 0 {
            return Err(ControlError::Busy);
        }
        let deadline = start.saturating_add((duration_ms as u64).saturating_mul(NS_PER_MS));
        while sched::now_ns_public() < deadline {
            if sched::is_ready() {
                sched::schedule_once(0);
            } else {
                core::hint::spin_loop();
            }
        }
        Ok(())
    }

    fn send_break(&self, duration_ms: u32) -> Result<(), ControlError> {
        self.flush().map_err(map_uart_char_error)?;
        self.set_break_condition(true);
        let result = self.wait_break_duration(duration_ms);
        self.set_break_condition(false);
        result
    }

    #[inline]
    fn enqueue_bytes(state: &mut UartTxState, buf: &[u8]) -> usize {
        let available = TX_SW_BUFFER_SIZE.saturating_sub(state.len);
        let count = available.min(buf.len());
        if count == 0 {
            return 0;
        }

        let first = count.min(TX_SW_BUFFER_SIZE - state.tail);
        state.buf[state.tail..state.tail + first].copy_from_slice(&buf[..first]);
        let second = count - first;
        if second != 0 {
            state.buf[..second].copy_from_slice(&buf[first..first + second]);
        }
        state.tail = (state.tail + count) % TX_SW_BUFFER_SIZE;
        state.len += count;
        count
    }

    #[inline]
    fn dequeue_byte(state: &mut UartTxState) -> u8 {
        let byte = state.buf[state.head];
        state.head = (state.head + 1) % TX_SW_BUFFER_SIZE;
        state.len -= 1;
        byte
    }

    fn kick_tx_locked(&self, state: &mut UartTxState) {
        while state.len != 0 {
            if !self.transmitter_fifo_ready() {
                break;
            }

            let count = state.len.min(TX_FIFO_BATCH);
            for _ in 0..count {
                let byte = Self::dequeue_byte(state);
                self.write_reg(REG_THR, byte);
            }
        }
    }

    fn service_tx(&self) {
        let Some(mut guard) = self.tx.try_lock() else {
            return;
        };
        let state = guard.state_mut();
        self.kick_tx_locked(state);
    }
}

// ─────────────────────────── CharDriver ──────────────────────────────────

impl CharDriver for Uart16550 {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_tty(&self) -> bool {
        true
    }

    fn is_console(&self) -> bool {
        true
    }

    fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
        if buf.is_empty() {
            return Ok(0);
        }

        // 部分写语义的调用方（write(2)）自己决定是否重试，这里返回 0 是合法的
        // "本次未写入"。真正需要原子性的整条消息走 write_all。
        let Some(mut guard) = self.tx.try_lock() else {
            return Ok(0);
        };
        let state = guard.state_mut();
        self.kick_tx_locked(state);
        let count = Self::enqueue_bytes(state, buf);
        self.kick_tx_locked(state);
        Ok(count)
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        // 整个"检查 DR + 弹出 RBR"序列必须在同一临界区内完成。此前这里完全没有
        // 互斥：多核下两个读者同时看到 DR=1，各自读走一个字节，一行输入就被拆散
        // 并乱序，宿主发给客机的命令因此损坏（`/tmp/p/` → `/tm/pp/`）。
        let _guard = self.lock_rx();
        for (i, slot) in buf.iter_mut().enumerate() {
            let lsr = self.read_reg(REG_LSR);
            if lsr & LSR_DR == 0 {
                return Ok(i);
            }
            *slot = self.read_reg(REG_RBR);
        }
        Ok(buf.len())
    }

    fn poll_read(&self) -> bool {
        self.line_status() & LSR_DR != 0
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, want_read: bool, _want_write: bool) -> bool {
        if !want_read {
            return false;
        }
        self.rx_wait.enqueue(task);
        true
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.rx_wait.remove(task);
    }

    fn flush(&self) -> Result<(), CharIoError> {
        let mut retries = 0usize;
        loop {
            {
                let mut guard = self.tx.lock();
                let state = guard.state_mut();
                self.kick_tx_locked(state);
                if state.len == 0 && self.transmitter_empty() {
                    return Ok(());
                }
            }

            retries += 1;
            if retries > TX_SPIN_RETRY_LIMIT {
                return Err(CharIoError::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    fn poll_write(&self) {
        self.service_tx();
    }

    fn control(&self, req: CharControlRequest) -> Result<CharControlResponse, ControlError> {
        match req {
            CharControlRequest::DrainTx => {
                self.flush().map_err(map_uart_char_error)?;
                Ok(CharControlResponse::Done)
            }
            CharControlRequest::FlushTx => {
                self.discard_tx_fifo_and_buffer();
                Ok(CharControlResponse::Done)
            }
            CharControlRequest::FlushRx => {
                self.discard_rx_fifo();
                Ok(CharControlResponse::Done)
            }
            CharControlRequest::FlushBoth => {
                self.discard_rx_tx();
                Ok(CharControlResponse::Done)
            }
            CharControlRequest::SendBreak { duration_ms } => {
                self.send_break(duration_ms)?;
                Ok(CharControlResponse::Done)
            }
            CharControlRequest::SetSerialConfig { baud: Some(baud) } => {
                let clock_hz = self.clock_hz.ok_or(ControlError::Unsupported)?;
                DriverControl::control(self, UartRequest::SetBaudRate { clock_hz, baud })
                    .map_err(map_uart_control_error)?;
                Ok(CharControlResponse::Done)
            }
            CharControlRequest::GetInputQueueLen => {
                Ok(CharControlResponse::U32(u32::from(self.poll_read())))
            }
            CharControlRequest::GetOutputQueueLen => {
                Ok(CharControlResponse::U32(self.queued_output_len()))
            }
            _ => Err(ControlError::Unsupported),
        }
    }

    fn write_all(&self, buf: &[u8]) -> Result<(), CharIoError> {
        // 整条消息必须在**同一个临界区**内写完。
        //
        // 之前这里每轮循环都重新取一次 tx 锁：TX ring 写满时（BuildStorm 这类
        // 高日志量负载下持续满）一条消息会被切成多段，另一个 CPU 的 write_all
        // 正好在段间插入自己的字节，串口上就出现字符级交织——例如
        // `/tmp/p/run.sh` 被打乱成 `/tm/pp/run.sh`。这会破坏宿主侧对串口的
        // 解析，让"捕获超时"这类假故障看起来像内核问题。
        //
        // 持锁期间靠 service_tx_locked 自己排空硬件 FIFO 推进，不释放锁，
        // 因此不存在与其它写者交错的窗口。
        let mut guard = self.tx.lock();
        let state = guard.state_mut();
        let mut remaining = buf;
        let mut retries = 0usize;
        while !remaining.is_empty() {
            self.kick_tx_locked(state);
            let written = Self::enqueue_bytes(state, remaining);
            self.kick_tx_locked(state);
            remaining = &remaining[written..];
            if written == 0 {
                retries += 1;
                if retries > TX_SPIN_RETRY_LIMIT {
                    return Err(CharIoError::Timeout);
                }
                core::hint::spin_loop();
            } else {
                retries = 0;
            }
        }
        Ok(())
    }
}

struct Uart16550IrqHandler {
    uart: Arc<Uart16550>,
}

impl IrqHandler for Uart16550IrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        self.uart.wake_rx_waiters_if_ready()
    }
}

struct Uart16550Binding {
    uart: Arc<Uart16550>,
}

fn map_irq_error(err: IrqError) -> PnpError {
    match err {
        IrqError::OutOfMemory => PnpError::OutOfMemory,
        IrqError::AlreadyRegistered => {
            PnpError::registration_failed(PnpResourceKind::Irq, "uart irq already registered")
        }
        IrqError::NotFound => {
            PnpError::registration_failed(PnpResourceKind::Irq, "uart irq line not found")
        }
    }
}

fn first_irq_dependency(info: &PlatformDeviceInfo) -> PnpDependency {
    info.irq_resources()
        .find_map(|irq| irq.controller())
        .map(PnpDependency::IrqController)
        .unwrap_or(PnpDependency::DefaultIrqDomain)
}

fn map_uart_register_config_error(error: UartRegisterConfigError) -> PnpError {
    log::warning!(
        "[platform-uart16550] rejected DT register layout: {:?}",
        error
    );
    PnpError::malformed(
        PnpResourceKind::Mmio,
        "invalid 8250 register layout or endianness",
    )
}

fn register_uart_irq(
    info: &PlatformDeviceInfo,
    uart: Arc<Uart16550>,
) -> Result<Option<IrqHandle>, PnpError> {
    let handler: Arc<dyn IrqHandler> = Arc::new(Uart16550IrqHandler {
        uart: Arc::clone(&uart),
    });
    match info.register_first_irq_handler(handler) {
        Ok(handle) => {
            uart.set_rx_irq_enabled(true);
            Ok(Some(handle))
        }
        Err(PlatformIrqRegistrationError::NoResource) => Ok(None),
        Err(PlatformIrqRegistrationError::Unresolved) => {
            log::debug!(
                "[platform-uart16550] {} has firmware IRQ resource but no registered IRQ domain translator",
                info.fw_name.as_ref()
            );
            Err(PnpError::dependency(first_irq_dependency(info)))
        }
        Err(PlatformIrqRegistrationError::RegistrationFailed { line, err }) => {
            log::printk!(
                "[platform-uart16550] failed to register irq {:?}: {:?}",
                line,
                map_irq_error(err)
            );
            uart.set_rx_irq_enabled(false);
            Err(map_irq_error(err))
        }
    }
}

// ──────────────────────── UART 控制接口 ───────────────────────────────────

/// UART 控制请求。
#[derive(Debug)]
pub enum UartRequest {
    /// 重新设置波特率。
    SetBaudRate { clock_hz: u32, baud: u32 },
    /// 读取线路状态寄存器（LSR）原始值。
    GetLineStatus,
}

/// UART 控制响应。
#[derive(Debug)]
pub enum UartResponse {
    /// 操作已完成，无附加数据。
    Done,
    /// LSR 原始字节。
    LineStatus(u8),
}

/// UART 控制错误。
#[derive(Debug)]
pub enum UartError {
    /// 波特率为 0。
    InvalidBaudRate,
}

fn map_uart_char_error(err: CharIoError) -> ControlError {
    match err {
        CharIoError::NoSpace => ControlError::Invalid,
        CharIoError::HardwareError => ControlError::Io,
        CharIoError::Unavailable => ControlError::NoDevice,
        CharIoError::Interrupted => ControlError::Busy,
        CharIoError::Timeout => ControlError::Busy,
    }
}

fn map_uart_control_error(err: UartError) -> ControlError {
    match err {
        UartError::InvalidBaudRate => ControlError::Invalid,
    }
}

impl DriverControl for Uart16550 {
    type Request = UartRequest;
    type Response = UartResponse;
    type Error = UartError;

    fn control(&self, req: UartRequest) -> Result<UartResponse, UartError> {
        match req {
            UartRequest::SetBaudRate { clock_hz, baud } => {
                let divisor = uart_divisor(clock_hz, baud).ok_or(UartError::InvalidBaudRate)?;
                let _ = self.flush();
                let dll = (divisor & 0xff) as u8;
                let dlm = (divisor >> 8) as u8;
                while !self.transmitter_empty() {
                    core::hint::spin_loop();
                }
                self.write_reg(REG_LCR, LCR_DLAB);
                self.write_reg(REG_DLL, dll);
                self.write_reg(REG_IER, dlm);
                self.write_reg(REG_LCR, LCR_8N1);
                Ok(UartResponse::Done)
            }
            UartRequest::GetLineStatus => {
                let lsr = self.read_reg(REG_LSR);
                Ok(UartResponse::LineStatus(lsr))
            }
        }
    }
}

// ──────────────────────── Platform PnP 绑定 ──────────────────────────────

/// NS16550A platform PnP 驱动。
///
/// 驱动只匹配固件枚举的串口节点，probe 时把 MMIO 基址映射为 [`Uart16550`]
/// 实例，并注册字符设备 function。
pub struct Uart16550PlatformDriver {
    device_mmio_to_virt: fn(usize) -> usize,
    projection_names: FunctionProjectionNameAllocator,
}

impl Uart16550PlatformDriver {
    /// 创建 platform 串口驱动。
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
            // 串口的用户可见节点名由兼容层分配器生成，驱动只声明“这是一个
            // 串口 function”，不把该名字作为硬件身份参与 PnP 匹配。
            projection_names: FunctionProjectionNameAllocator::new("uart"),
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        // DW_APB 串口（JH7110 等）由 platform-jh7110-uart 专属驱动接管，
        // 避免两个内建驱动同时匹配同一节点触发 DriverAmbiguous。
        info.has_id("ns16550")
            || info.has_id("ns16550a")
            || info.has_id("PNP0500")
            || info.has_id("PNP0501")
    }
}

impl PnpDriver for Uart16550PlatformDriver {
    fn name(&self) -> &'static str {
        "platform-uart16550"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn crate::dev::pnp::PnpBusInfo) -> bool {
        if !matches!(id, PnpId::Platform { .. }) {
            return false;
        }
        info.as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &alloc::sync::Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let Some((phys, size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "uart reg missing"));
        };
        let virt_base = (self.device_mmio_to_virt)(phys);
        let registers = UartRegisterAccess::from_platform(info, virt_base, size)
            .map_err(map_uart_register_config_error)?;
        let provider_clock = uart_provider_clock(info)?;
        let clock_hz = provider_clock
            .as_ref()
            .map(|(rate, _)| *rate)
            .or(info.properties.clock_hz);
        let uart = if let Some(clock_hz) = clock_hz {
            Arc::new(
                Uart16550::new(
                    registers,
                    clock_hz,
                    info.properties.baud.unwrap_or(UART_DEFAULT_BAUD),
                )
                .map_err(|_| {
                    PnpError::malformed(
                        PnpResourceKind::Other("clock"),
                        "uart clock and baud produce an invalid divisor",
                    )
                })?,
            )
        } else {
            Arc::new(Uart16550::new_preconfigured(registers))
        };
        if let Some((_, lease)) = provider_clock {
            dev.own_resource(dt_provider::lease_pnp_resource(
                lease,
                "platform-uart16550-clock",
            ))?;
        }

        let dev_name = self
            .projection_names
            .try_alloc_stable(&dev.name)?
            .into_string();
        let ch = CharDevice::from_arc(
            info.fw_name.clone(),
            Arc::clone(&uart) as Arc<dyn CharDriver>,
        );
        let irq_handle = register_uart_irq(info, Arc::clone(&uart))?;
        if let Some(handle) = irq_handle
            && let Err(err) = dev.own_resource(irq::irq_handler_pnp_resource(
                handle,
                "platform-uart16550-rx",
            ))
        {
            uart.set_rx_irq_enabled(false);
            let _ = irq::unregister_irq_handler(handle);
            return Err(err);
        }
        if let Err(err) = dev.register_function(CharFunction::with_projection_name_arc(
            &dev.name, &dev_name, ch,
        )) {
            uart.set_rx_irq_enabled(false);
            return Err(err);
        }
        dev.set_driver_data(Arc::new(Uart16550Binding {
            uart: Arc::clone(&uart),
        }));
        log::printk!(
            "[platform-uart16550] bound {} phys={:#x} reg-offset={:#x} reg-shift={} reg-io-width={} endian={:?} -> /dev/{}",
            dev.id,
            phys,
            registers.reg_offset,
            registers.reg_shift,
            registers.width.bytes(),
            registers.endian,
            dev_name
        );
        Ok(())
    }

    fn remove(&self, dev: &alloc::sync::Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<Uart16550Binding>()
        {
            binding.uart.set_rx_irq_enabled(false);
        }
        log::printk!("[platform-uart16550] removed {}", dev.id);
    }
}

struct Uart16550Factory;

impl DriverFactory for Uart16550Factory {
    fn name(&self) -> &'static str {
        "platform-uart16550"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Uart16550PlatformDriver::new(
            ctx.device_mmio_to_virt,
        )))
    }
}

/// 注册 NS16550A platform 内建驱动 factory。
pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Uart16550Factory))
}