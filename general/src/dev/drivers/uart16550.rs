//! NS16550A 兼容 UART 驱动程序。
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
use crate::dev::function::{CharFunction, FunctionProjectionNameAllocator};
use crate::dev::irq::{self, IrqError, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use crate::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpDependency, PnpDevice, PnpDriver, PnpError, PnpId,
    PnpResourceKind, register_driver_factory,
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
/// 软件发送缓冲区大小。
const TX_SW_BUFFER_SIZE: usize = 32 * 1024;
/// flush/write_all 等待硬件发送完成时的自旋上限。
const TX_SPIN_RETRY_LIMIT: usize = 10_000_000;
/// break 保持时间换算。
const NS_PER_MS: u64 = 1_000_000;

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
        unsafe { &mut *self.buffer.state.get() }
    }
}

impl Drop for UartTxGuard<'_> {
    fn drop(&mut self) {
        self.buffer.lock.store(0, Ordering::Release);
    }
}

// ─────────────────────────── Uart16550 ────────────────────────────────────

/// NS16550A 兼容 UART 驱动（内存映射 I/O）。
pub struct Uart16550 {
    /// UART 寄存器组的虚拟基地址。
    base: usize,
    /// UART 输入时钟；固件预配置路径可能没有稳定来源。
    clock_hz: Option<u32>,
    /// 软件发送缓冲区。日志路径先入队，再由轮询/flush 持续排空到 UART FIFO。
    tx: UartTxBuffer,
    /// RX 等待队列。硬件中断只负责唤醒，真正读取和行规程仍在 VFS/TTY 路径完成。
    rx_wait: WaitQueue,
}

unsafe impl Send for Uart16550 {}
unsafe impl Sync for Uart16550 {}

impl Uart16550 {
    /// 创建并初始化一个 UART 驱动实例。
    ///
    /// - `virt_base`: UART 寄存器的基地址。
    /// - `clock_hz`: UART 输入时钟频率（Hz）。
    /// - `baud`: 目标波特率（如 `115_200`）。
    pub fn new(virt_base: usize, clock_hz: u32, baud: u32) -> Self {
        let uart = Self {
            base: virt_base,
            clock_hz: Some(clock_hz),
            tx: UartTxBuffer::new(),
            rx_wait: WaitQueue::new(),
        };
        uart.init(clock_hz, baud);
        uart
    }

    /// 接管固件已经初始化过的 UART，不重新编程 divisor。
    ///
    /// ACPI SPCR 允许固件描述一个已经用于 console redirection 的串口，但旧版
    /// SPCR 不一定提供 UART 输入时钟。此路径只关闭中断并启用 FIFO/握手，避免
    /// 引入平台默认时钟 fallback。
    pub fn new_preconfigured(virt_base: usize) -> Self {
        let uart = Self {
            base: virt_base,
            clock_hz: None,
            tx: UartTxBuffer::new(),
            rx_wait: WaitQueue::new(),
        };
        uart.attach_preconfigured();
        uart
    }

    /// 返回 UART 寄存器组的虚拟基地址。
    #[inline]
    pub fn base(&self) -> usize {
        self.base
    }

    #[inline]
    fn reg(&self, offset: usize) -> *mut u8 {
        (self.base + offset) as *mut u8
    }

    #[inline]
    fn line_status(&self) -> u8 {
        unsafe { core::ptr::read_volatile(self.reg(REG_LSR)) }
    }

    #[inline]
    fn transmitter_fifo_ready(&self) -> bool {
        self.line_status() & LSR_THRE != 0
    }

    #[inline]
    fn transmitter_empty(&self) -> bool {
        self.line_status() & LSR_TEMT != 0
    }

    fn init(&self, clock_hz: u32, baud: u32) {
        let divisor = if baud == 0 {
            1u16
        } else {
            (clock_hz / (UART_DIVISOR_OVERSAMPLE * baud)) as u16
        };
        let dll = (divisor & 0xff) as u8;
        let dlm = (divisor >> 8) as u8;

        unsafe {
            core::ptr::write_volatile(self.reg(REG_IER), 0x00);
            core::ptr::write_volatile(self.reg(REG_LCR), LCR_DLAB);
            core::ptr::write_volatile(self.reg(REG_DLL), dll);
            core::ptr::write_volatile(self.reg(REG_IER), dlm);
            core::ptr::write_volatile(self.reg(REG_LCR), LCR_8N1);
            core::ptr::write_volatile(self.reg(REG_FCR), FCR_ENABLE_FIFO | FCR_CLEAR_RXTX);
            core::ptr::write_volatile(self.reg(REG_MCR), MCR_DTR_RTS);
        }
    }

    fn attach_preconfigured(&self) {
        unsafe {
            // 预配置串口通常已经被固件接到 QEMU stdio/console。保持 RX 可用
            // 中断使能，避免后端因 guest 侧未声明可接收而不再投递输入；实际
            // 消费仍走本驱动的轮询 read()，不依赖中断处理路径。
            core::ptr::write_volatile(self.reg(REG_IER), IER_RDI);
            core::ptr::write_volatile(self.reg(REG_FCR), FCR_ENABLE_FIFO);
            core::ptr::write_volatile(self.reg(REG_MCR), MCR_DTR_RTS);
        }
    }

    fn set_rx_irq_enabled(&self, enabled: bool) {
        let value = if enabled { IER_RDI } else { 0 };
        unsafe {
            // 本驱动只使用 RX-ready 中断；关闭时直接写 0，避免保留未知固件残留位。
            core::ptr::write_volatile(self.reg(REG_IER), value);
        }
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
        unsafe {
            core::ptr::write_volatile(self.reg(REG_FCR), FCR_ENABLE_FIFO | FCR_CLEAR_RX);
        }
        // FCR 清空后再读掉残留的 Data Ready，覆盖实现存在延迟的 UART 变体。
        for _ in 0..FIFO_DEPTH {
            if self.line_status() & LSR_DR == 0 {
                break;
            }
            let _ = unsafe { core::ptr::read_volatile(self.reg(REG_RBR)) };
        }
    }

    fn discard_tx_fifo_and_buffer(&self) {
        let mut guard = self.tx.lock();
        let state = guard.state_mut();
        state.head = 0;
        state.tail = 0;
        state.len = 0;
        unsafe {
            core::ptr::write_volatile(self.reg(REG_FCR), FCR_ENABLE_FIFO | FCR_CLEAR_TX);
        }
    }

    fn discard_rx_tx(&self) {
        {
            let mut guard = self.tx.lock();
            let state = guard.state_mut();
            state.head = 0;
            state.tail = 0;
            state.len = 0;
            unsafe {
                core::ptr::write_volatile(self.reg(REG_FCR), FCR_ENABLE_FIFO | FCR_CLEAR_RXTX);
            }
        }
        for _ in 0..FIFO_DEPTH {
            if self.line_status() & LSR_DR == 0 {
                break;
            }
            let _ = unsafe { core::ptr::read_volatile(self.reg(REG_RBR)) };
        }
    }

    fn set_break_condition(&self, enabled: bool) {
        let mut lcr = unsafe { core::ptr::read_volatile(self.reg(REG_LCR)) };
        if enabled {
            lcr |= LCR_BREAK;
        } else {
            lcr &= !LCR_BREAK;
        }
        unsafe {
            core::ptr::write_volatile(self.reg(REG_LCR), lcr);
        }
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

            let count = state.len.min(FIFO_DEPTH);
            for _ in 0..count {
                let byte = Self::dequeue_byte(state);
                unsafe {
                    core::ptr::write_volatile(self.reg(REG_THR), byte);
                }
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
        for (i, slot) in buf.iter_mut().enumerate() {
            let lsr = unsafe { core::ptr::read_volatile(self.reg(REG_LSR)) };
            if lsr & LSR_DR == 0 {
                return Ok(i);
            }
            *slot = unsafe { core::ptr::read_volatile(self.reg(REG_RBR)) };
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
        let mut remaining = buf;
        let mut retries = 0usize;
        while !remaining.is_empty() {
            let written = {
                let mut guard = self.tx.lock();
                let state = guard.state_mut();
                self.kick_tx_locked(state);
                let count = Self::enqueue_bytes(state, remaining);
                self.kick_tx_locked(state);
                count
            };

            remaining = &remaining[written..];
            if written == 0 {
                retries += 1;
                if retries > TX_SPIN_RETRY_LIMIT {
                    return Err(CharIoError::Timeout);
                }
                self.service_tx();
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
                if baud == 0 {
                    return Err(UartError::InvalidBaudRate);
                }
                let _ = self.flush();
                let divisor = (clock_hz / (UART_DIVISOR_OVERSAMPLE * baud)) as u16;
                let dll = (divisor & 0xff) as u8;
                let dlm = (divisor >> 8) as u8;
                unsafe {
                    while !self.transmitter_empty() {
                        core::hint::spin_loop();
                    }
                    core::ptr::write_volatile(self.reg(REG_LCR), LCR_DLAB);
                    core::ptr::write_volatile(self.reg(REG_DLL), dll);
                    core::ptr::write_volatile(self.reg(REG_IER), dlm);
                    core::ptr::write_volatile(self.reg(REG_LCR), LCR_8N1);
                }
                Ok(UartResponse::Done)
            }
            UartRequest::GetLineStatus => {
                let lsr = unsafe { core::ptr::read_volatile(self.reg(REG_LSR)) };
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
        let Some((phys, _size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "uart reg missing"));
        };
        let virt_base = (self.device_mmio_to_virt)(phys);
        let uart = if let Some(clock_hz) = info.properties.clock_hz {
            Arc::new(Uart16550::new(
                virt_base,
                clock_hz,
                info.properties.baud.unwrap_or(UART_DEFAULT_BAUD),
            ))
        } else {
            Arc::new(Uart16550::new_preconfigured(virt_base))
        };

        let dev_name = self
            .projection_names
            .try_alloc_stable(&dev.name)?
            .into_string();
        let ch = CharDevice::new(info.fw_name.clone(), Arc::clone(&uart));
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
        if let Err(err) = dev.register_function(Arc::new(CharFunction::with_projection_name(
            &dev.name, &dev_name, ch,
        ))) {
            uart.set_rx_irq_enabled(false);
            return Err(err);
        }
        dev.set_driver_data(Arc::new(Uart16550Binding {
            uart: Arc::clone(&uart),
        }));
        log::printk!(
            "[platform-uart16550] bound {} phys={:#x} -> /dev/{}",
            dev.id,
            phys,
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
pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(Uart16550Factory)).map(|_| ())
}
