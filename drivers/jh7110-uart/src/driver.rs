//! StarFive JH7110 / DesignWare APB UART 控制台驱动。
//!
//! 完整实现 DW_APB_UART 的 16550 兼容子集：
//! - 支持 DT 的 reg-shift / reg-io-width / 端序属性；
//! - 通过 dt_provider 向 JH7110 CRG 查询 baudclk 输入时钟（UART0_CORE=24 MHz）；
//! - 注册字符设备 function 作为系统控制台（/dev/uartN + console）；
//! - 接收中断经 PLIC 接入，唤醒读等待者；
//! - probe 时应用 pinctrl-0 默认引脚状态（sys pinctrl 按 pinmux 值编程）。
//!
//! 与 platform.uart16550 分工：本驱动独占 starfive,jh7110-uart /
//! snps,dw-apb-uart 兼容串口；uart16550 只匹配 ns16550 系列。

use alloc::sync::Arc;
use core::any::Any;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use sched::{Task, WaitQueue};

use crate::dev::char::*;
use crate::dev::dt_provider;
use crate::dev::function::{CharFunction, FunctionProjectionNameAllocator};
use crate::dev::irq::{self, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use crate::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpDependency, PnpDevice, PnpDriver,
    PnpDriverPriority, PnpError, PnpId, PnpResourceKind, register_driver_factory,
    register_function as register_pnp_function,
};

// ── 寄存器布局 ──
const REG_THR: usize = 0;
const REG_RBR: usize = 0;
const REG_DLL: usize = 0;
const REG_IER: usize = 1;
const REG_FCR: usize = 2;
const REG_LCR: usize = 3;
const REG_MCR: usize = 4;
const REG_LSR: usize = 5;

const LSR_DR: u8 = 1 << 0;
const LSR_THRE: u8 = 1 << 5;
const LSR_TEMT: u8 = 1 << 6;
const IER_RDI: u8 = 1 << 0;
const LCR_DLAB: u8 = 1 << 7;
const LCR_8N1: u8 = 0x03;
const FCR_ENABLE_FIFO: u8 = 0x01;
const FCR_CLEAR_RXTX: u8 = 0x06;
const MCR_DTR_RTS: u8 = 0x03;

const DEFAULT_BAUD: u32 = 115_200;
const TX_BUF_SIZE: usize = 32 * 1024;
const TX_SPIN_RETRY: usize = 10_000_000;

const CLOCK_PROPERTY: &str = "clocks";
const CLOCK_BAUD_NAME: &str = "baudclk";

fn divisor(clock_hz: u32, baud: u32) -> Option<u16> {
    let d = clock_hz.checked_div(baud.checked_mul(16)?)?;
    (d != 0 && d <= u32::from(u16::MAX)).then_some(d as u16)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegWidth {
    U8,
    U16,
    U32,
}

impl RegWidth {
    const fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }
}

#[derive(Clone, Copy)]
struct RegAccess {
    base: usize,
    shift: u32,
    width: RegWidth,
}

impl RegAccess {
    fn from_platform(info: &PlatformDeviceInfo, base: usize) -> Result<Self, PnpError> {
        let shift = optional_u32(info, "reg-shift").unwrap_or(0);
        let width = match optional_u32(info, "reg-io-width").unwrap_or(1) {
            1 => RegWidth::U8,
            2 => RegWidth::U16,
            4 => RegWidth::U32,
            _ => {
                return Err(PnpError::malformed(
                    PnpResourceKind::Mmio,
                    "invalid reg-io-width",
                ));
            }
        };
        if shift >= usize::BITS {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "invalid reg-shift",
            ));
        }
        if (1usize << shift) < width.bytes() {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "misaligned regs",
            ));
        }
        Ok(Self { base, shift, width })
    }

    fn addr(self, reg: usize) -> usize {
        self.base + (reg << self.shift)
    }

    fn read(self, reg: usize) -> u8 {
        // Safety: probe 已按 reg-shift/reg-io-width 校验布局，窗口在 DT reg 内。
        unsafe {
            let address = self.addr(reg);
            match self.width {
                RegWidth::U8 => core::ptr::read_volatile(address as *const u8),
                RegWidth::U16 => {
                    u16::from_le(core::ptr::read_volatile(address as *const u16)) as u8
                }
                RegWidth::U32 => {
                    u32::from_le(core::ptr::read_volatile(address as *const u32)) as u8
                }
            }
        }
    }

    fn write(self, reg: usize, value: u8) {
        // Safety: 同 read。
        unsafe {
            let address = self.addr(reg);
            match self.width {
                RegWidth::U8 => core::ptr::write_volatile(address as *mut u8, value),
                RegWidth::U16 => {
                    core::ptr::write_volatile(address as *mut u16, u16::from(value).to_le())
                }
                RegWidth::U32 => {
                    core::ptr::write_volatile(address as *mut u32, u32::from(value).to_le())
                }
            }
        }
    }
}

fn optional_u32(info: &PlatformDeviceInfo, name: &str) -> Option<u32> {
    let raw = info.bytes_property(name)?;
    let bytes: [u8; 4] = raw.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn uart_clock(dev: &Arc<PnpDevice>, info: &PlatformDeviceInfo) -> Result<Option<u32>, PnpError> {
    let reference = info
        .dtb_reference_by_name(CLOCK_PROPERTY, CLOCK_BAUD_NAME)
        .or_else(|| info.dtb_references(CLOCK_PROPERTY).next());
    let Some(reference) = reference else {
        return Ok(None);
    };
    let rate = dt_provider::acquire_reference_rate_for_device(dev, reference, "jh7110-uart-clock")?;
    let rate = u32::try_from(rate).map_err(|_| {
        PnpError::malformed(PnpResourceKind::Other("clock"), "clock rate too large")
    })?;
    if rate == 0 {
        return Err(PnpError::malformed(
            PnpResourceKind::Other("clock"),
            "zero clock rate",
        ));
    }
    Ok(Some(rate))
}

fn apply_default_pinctrl(dev: &Arc<PnpDevice>, info: &PlatformDeviceInfo) -> Result<(), PnpError> {
    // pinctrl 状态应用失败不阻塞控制台绑定：引脚已由固件配置，
    // provider 未注册或配置不完整时仅记录并继续。
    let Some(reference) = info
        .dtb_reference_by_name("pinctrl-0", "default")
        .or_else(|| info.dtb_references("pinctrl-0").next())
    else {
        return Ok(());
    };
    if let Err(error) = dt_provider::acquire_reference_configure_for_device(
        dev,
        reference,
        &[],
        "jh7110-uart-pinctrl",
    ) {
        log::warning!(
            "[jh7110-uart] default pinctrl configure failed: {:?}",
            error
        );
    }
    Ok(())
}

struct TxState {
    buf: [u8; TX_BUF_SIZE],
    head: usize,
    tail: usize,
    len: usize,
}

impl TxState {
    const fn new() -> Self {
        Self {
            buf: [0; TX_BUF_SIZE],
            head: 0,
            tail: 0,
            len: 0,
        }
    }
}

struct TxBuffer {
    state: UnsafeCell<TxState>,
    lock: AtomicUsize,
}

unsafe impl Sync for TxBuffer {}

impl TxBuffer {
    const fn new() -> Self {
        Self {
            state: UnsafeCell::new(TxState::new()),
            lock: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> TxGuard<'_> {
        while self
            .lock
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            core::hint::spin_loop();
        }
        TxGuard { buffer: self }
    }
}

struct TxGuard<'a> {
    buffer: &'a TxBuffer,
}

impl TxGuard<'_> {
    fn state_mut(&mut self) -> &mut TxState {
        // Safety: 独占自旋锁，引用不越过守卫生命周期。
        unsafe { &mut *self.buffer.state.get() }
    }
}

impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        self.buffer.lock.store(0, Ordering::Release);
    }
}

pub struct Jh7110Uart {
    regs: RegAccess,
    tx: TxBuffer,
    rx_wait: WaitQueue,
    rx_lock: AtomicUsize,
}

unsafe impl Send for Jh7110Uart {}
unsafe impl Sync for Jh7110Uart {}

impl Jh7110Uart {
    fn new(regs: RegAccess, clock_hz: u32, baud: u32) -> Result<Self, PnpError> {
        let divisor = divisor(clock_hz, baud).ok_or(PnpError::malformed(
            PnpResourceKind::Other("clock"),
            "invalid baud divisor",
        ))?;
        let uart = Self {
            regs,
            tx: TxBuffer::new(),
            rx_wait: WaitQueue::new(),
            rx_lock: AtomicUsize::new(0),
        };
        uart.init(divisor);
        Ok(uart)
    }

    /// 接管固件已配置的串口（无时钟来源时）。
    fn preconfigured(regs: RegAccess) -> Self {
        let uart = Self {
            regs,
            tx: TxBuffer::new(),
            rx_wait: WaitQueue::new(),
            rx_lock: AtomicUsize::new(0),
        };
        uart.regs.write(REG_IER, IER_RDI);
        uart.regs.write(REG_FCR, FCR_ENABLE_FIFO);
        uart.regs.write(REG_MCR, MCR_DTR_RTS);
        uart
    }

    fn init(&self, divisor: u16) {
        let dll = (divisor & 0xff) as u8;
        let dlm = (divisor >> 8) as u8;
        self.regs.write(REG_IER, 0);
        self.regs.write(REG_LCR, LCR_DLAB);
        self.regs.write(REG_DLL, dll);
        self.regs.write(REG_IER, dlm);
        self.regs.write(REG_LCR, LCR_8N1);
        self.regs.write(REG_FCR, FCR_ENABLE_FIFO | FCR_CLEAR_RXTX);
        self.regs.write(REG_MCR, MCR_DTR_RTS);
    }

    fn lsr(&self) -> u8 {
        self.regs.read(REG_LSR)
    }

    fn set_rx_irq_enabled(&self, enabled: bool) {
        self.regs.write(REG_IER, if enabled { IER_RDI } else { 0 });
    }

    fn kick_tx(&self, state: &mut TxState) {
        while state.len != 0 {
            if self.lsr() & LSR_THRE == 0 {
                break;
            }
            let byte = state.buf[state.head];
            state.head = (state.head + 1) % TX_BUF_SIZE;
            state.len -= 1;
            self.regs.write(REG_THR, byte);
        }
    }

    fn lock_rx(&self) {
        while self
            .rx_lock
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            core::hint::spin_loop();
        }
    }

    fn unlock_rx(&self) {
        self.rx_lock.store(0, Ordering::Release);
    }
}

impl CharDriver for Jh7110Uart {
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
        let mut guard = self.tx.lock();
        let state = guard.state_mut();
        self.kick_tx(state);
        let available = TX_BUF_SIZE - state.len;
        let count = available.min(buf.len());
        let first = count.min(TX_BUF_SIZE - state.tail);
        state.buf[state.tail..state.tail + first].copy_from_slice(&buf[..first]);
        if first < count {
            state.buf[..count - first].copy_from_slice(&buf[first..count]);
        }
        state.tail = (state.tail + count) % TX_BUF_SIZE;
        state.len += count;
        self.kick_tx(state);
        Ok(count)
    }

    fn write_all(&self, buf: &[u8]) -> Result<(), CharIoError> {
        let mut guard = self.tx.lock();
        let state = guard.state_mut();
        let mut remaining = buf;
        let mut retries = 0usize;
        // 直到全部字节拷入环且环被硬件排空（THRE 停摆时按超时失败，
        // 避免返回 Ok 但字节永远滞留环内——此前用户态 write 即因此丢字）。
        while !remaining.is_empty() || state.len != 0 {
            self.kick_tx(state);
            if state.len != 0 {
                retries += 1;
                if retries > TX_SPIN_RETRY {
                    return Err(CharIoError::Timeout);
                }
                core::hint::spin_loop();
                continue;
            }
            retries = 0;
            let available = TX_BUF_SIZE - state.len;
            let count = available.min(remaining.len());
            if count == 0 {
                continue;
            }
            let first = count.min(TX_BUF_SIZE - state.tail);
            state.buf[state.tail..state.tail + first].copy_from_slice(&remaining[..first]);
            if first < count {
                state.buf[..count - first].copy_from_slice(&remaining[first..count]);
            }
            state.tail = (state.tail + count) % TX_BUF_SIZE;
            state.len += count;
            remaining = &remaining[count..];
        }
        Ok(())
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        self.lock_rx();
        for (i, slot) in buf.iter_mut().enumerate() {
            if self.lsr() & LSR_DR == 0 {
                self.unlock_rx();
                return Ok(i);
            }
            *slot = self.regs.read(REG_RBR);
        }
        self.unlock_rx();
        Ok(buf.len())
    }

    fn poll_read(&self) -> bool {
        self.lsr() & LSR_DR != 0
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
                self.kick_tx(state);
                if state.len == 0 && self.lsr() & LSR_TEMT != 0 {
                    return Ok(());
                }
            }
            retries += 1;
            if retries > TX_SPIN_RETRY {
                return Err(CharIoError::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    fn poll_write(&self) {
        let mut guard = self.tx.lock();
        self.kick_tx(guard.state_mut());
    }
}

struct Jh7110UartIrqHandler {
    uart: Arc<Jh7110Uart>,
}

impl IrqHandler for Jh7110UartIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if self.uart.poll_read() {
            self.uart.rx_wait.wake_all();
            IrqStatus::Handled
        } else {
            IrqStatus::Unhandled
        }
    }
}

struct Jh7110UartBinding {
    uart: Arc<Jh7110Uart>,
}

fn map_irq_error(err: irq::IrqError) -> PnpError {
    match err {
        irq::IrqError::OutOfMemory => PnpError::OutOfMemory,
        irq::IrqError::AlreadyRegistered => {
            PnpError::registration_failed(PnpResourceKind::Irq, "irq already registered")
        }
        irq::IrqError::NotFound => {
            PnpError::registration_failed(PnpResourceKind::Irq, "irq line not found")
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
    uart: Arc<Jh7110Uart>,
) -> Result<Option<IrqHandle>, PnpError> {
    let handler: Arc<dyn IrqHandler> = Arc::new(Jh7110UartIrqHandler {
        uart: Arc::clone(&uart),
    });
    match info.register_first_irq_handler(handler) {
        Ok(handle) => {
            uart.set_rx_irq_enabled(true);
            Ok(Some(handle))
        }
        Err(PlatformIrqRegistrationError::NoResource) => Ok(None),
        Err(PlatformIrqRegistrationError::Unresolved) => {
            Err(PnpError::dependency(first_irq_dependency(info)))
        }
        Err(PlatformIrqRegistrationError::RegistrationFailed { line, err }) => {
            log::printk!(
                "[jh7110-uart] failed to register irq {:?}: {:?}",
                line,
                map_irq_error(err)
            );
            Err(map_irq_error(err))
        }
    }
}

pub struct Jh7110UartPlatformDriver {
    device_mmio_to_virt: fn(usize) -> usize,
    projection_names: FunctionProjectionNameAllocator,
}

impl Jh7110UartPlatformDriver {
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
            projection_names: FunctionProjectionNameAllocator::new("uart"),
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id("starfive,jh7110-uart") || info.has_id("snps,dw-apb-uart")
    }
}

impl PnpDriver for Jh7110UartPlatformDriver {
    fn name(&self) -> &'static str {
        "platform-jh7110-uart"
    }

    fn priority(&self) -> PnpDriverPriority {
        // 平台专属驱动：与通用 8250 并存时优先绑定 DW_APB 串口。
        PnpDriverPriority::SPECIFIC
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
        let (phys, size) = info
            .first_mmio()
            .ok_or(PnpError::missing(PnpResourceKind::Mmio, "uart reg missing"))?;
        let virt_base = (self.device_mmio_to_virt)(phys);
        let regs = RegAccess::from_platform(info, virt_base)?;
        // 校验窗口大小（reg 属性给出的 size）。
        if size != 0 {
            let last = regs.addr(REG_LSR) + regs.width.bytes();
            if last > virt_base + size {
                return Err(PnpError::malformed(
                    PnpResourceKind::Mmio,
                    "reg window too small",
                ));
            }
        }

        let clock_hz = uart_clock(dev, info)?;
        let uart = match clock_hz {
            Some(clock_hz) => Arc::new(Jh7110Uart::new(
                regs,
                clock_hz,
                info.properties.baud.unwrap_or(DEFAULT_BAUD),
            )?),
            None => Arc::new(Jh7110Uart::preconfigured(regs)),
        };

        // 应用默认 pinctrl 状态（引脚已由固件配置时等价幂等）。
        apply_default_pinctrl(dev, info)?;

        let dev_name = self
            .projection_names
            .try_alloc_stable(&dev.name)?
            .into_string();
        let irq_handle = register_uart_irq(info, Arc::clone(&uart))?;
        if let Some(handle) = irq_handle
            && let Err(err) =
                dev.own_resource(irq::irq_handler_pnp_resource(handle, "jh7110-uart-rx"))
        {
            uart.set_rx_irq_enabled(false);
            let _ = irq::unregister_irq_handler(handle);
            return Err(err);
        }
        if let Err(err) = register_pnp_function(
            dev,
            CharFunction::from_driver_arc(
                info.fw_name.clone(),
                Arc::clone(&uart) as Arc<dyn CharDriver>,
                &dev.name,
                &dev_name,
            ),
        ) {
            uart.set_rx_irq_enabled(false);
            return Err(err);
        }
        dev.set_driver_data(Arc::new(Jh7110UartBinding { uart }));
        log::printk!(
            "[jh7110-uart] bound {} phys={:#x} shift={} width={} clock={} -> /dev/{}",
            dev.id,
            phys,
            regs.shift,
            regs.width.bytes(),
            clock_hz.unwrap_or(0),
            dev_name
        );
        Ok(())
    }

    fn remove(&self, dev: &alloc::sync::Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<Jh7110UartBinding>()
        {
            binding.uart.set_rx_irq_enabled(false);
        }
        log::printk!("[jh7110-uart] removed {}", dev.id);
    }
}

struct Jh7110UartFactory;

impl DriverFactory for Jh7110UartFactory {
    fn name(&self) -> &'static str {
        "platform-jh7110-uart"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Jh7110UartPlatformDriver::new(
            ctx.device_mmio_to_virt,
        )))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Jh7110UartFactory))
}
