//! LS2K1000 I2C 控制器 platform ELM 驱动。
//!
//! 匹配工厂 DTB 的 `loongson,ls-i2c` 节点（reg 0x1fe21000 / 0x1fe21800
//! 窗口 0x08，clocks=<&clk 12> APB 时钟）。控制器是经典 ls2x 设计
//! （与 Linux drivers/i2c/busses/i2c-ls2x.c 一致）：
//!
//! - PRER_LO/HI（0x0/0x1）：时钟预分频 = PCLK/(5×总线频率) - 1；
//! - CTR（0x2）：[7] EN、[6] IEN、[5] MST；先写 0 进入频率设置模式，
//!   再写 0xE0 进入正常模式；
//! - TXR/RXR（0x3）：发送/接收数据；
//! - CR（0x4）：命令字节（START/STOP/READ/WRITE/ACK/IACK），写入即发起
//!   一次 8 位传输；
//! - SR（0x4）：状态（NOACK/BUSY/AL/TIP/IF）。
//!
//! 传输用轮询 IF + 超时完成（中断线未接线到本驱动），并对外暴露一个
//! `mygo.device.i2c-bus@1` 设备函数（read/write/read_regs/write_regs），
//! 供未来 gt911 触摸屏或传感器驱动消费。当前工厂 DTB 中 i2c0 为
//! disabled，只有 i2c@1fe21800（irq 0x17）处于 okay 状态。

use alloc::sync::Arc;

use general::dev::dt_provider::{DtbProviderError, DtbResourceReply, DtbResourceRequest};
use general::dev::function::{DeviceClassId, DeviceFunction, DeviceFunctionInvokeError};
use general::dev::platform::PlatformDeviceInfo;
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, register_driver_factory,
};
use vfs::sync::Spinlock;

const COMPAT_LOONGSON_LS_I2C: &str = "loongson,ls-i2c";
const PROP_CLOCKS: &str = "clocks";

// 寄存器偏移（8 位寄存器）。
const PRER_LO_REG: usize = 0x00;
const PRER_HI_REG: usize = 0x01;
const CTR_REG: usize = 0x02;
const TXR_REG: usize = 0x03;
const RXR_REG: usize = 0x03;
const CR_REG: usize = 0x04;
const SR_REG: usize = 0x04;

const CTR_EN: u8 = 1 << 7;
const CTR_IEN: u8 = 1 << 6;
const CTR_MST: u8 = 1 << 5;
const CTR_READY_MASK: u8 = CTR_EN | CTR_IEN | CTR_MST;
const CTR_FREQ_MASK: u8 = 0xc0;

const CR_START: u8 = 1 << 7;
const CR_STOP: u8 = 1 << 6;
const CR_READ: u8 = 1 << 5;
const CR_WRITE: u8 = 1 << 4;
const CR_ACK: u8 = 1 << 3;
const CR_IACK: u8 = 1 << 0;

const SR_NOACK: u8 = 1 << 7;
const SR_BUSY: u8 = 1 << 6;
const SR_AL: u8 = 1 << 5;
const SR_IF: u8 = 1 << 0;

/// 默认总线频率（Linux 经验值）。
const DEFAULT_BUS_HZ: u32 = 33_000;
const XFER_TIMEOUT_LOOPS: u32 = 50_000;
const STOP_TIMEOUT_LOOPS: u32 = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ls2kI2cError {
    Timeout,
    NoAck,
    ArbitrationLost,
    Invalid,
}

fn delay_ns(duration_ns: u64) {
    let deadline = hal::time::monotonic_ns().saturating_add(duration_ns);
    while hal::time::monotonic_ns() < deadline {
        core::hint::spin_loop();
    }
}

/// LS2X I2C 控制器实例。
pub struct Ls2xI2cBus {
    base: usize,
    lock: Spinlock<()>,
}

impl Ls2xI2cBus {
    fn new(base: usize, clk_rate: u64) -> Self {
        let bus = Self {
            base,
            lock: Spinlock::new(()),
        };
        bus.init(clk_rate);
        bus
    }

    fn read8(&self, offset: usize) -> u8 {
        // Safety: offset 是受控固定寄存器偏移，base 由 platform probe 映射。
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u8) }
    }

    fn write8(&self, offset: usize, value: u8) {
        // Safety: 同 read8，目标寄存器允许 8 位易失写入。
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u8, value) }
    }

    /// 控制器初始化（Linux ls2x_i2c_init）。
    fn init(&self, clk_rate: u64) {
        self.write8(CTR_REG, self.read8(CTR_REG) & !CTR_FREQ_MASK);
        let clk = if clk_rate == 0 { 50_000_000 } else { clk_rate };
        let prescale = (clk / (5 * u64::from(DEFAULT_BUS_HZ)))
            .saturating_sub(1)
            .min(0xffff);
        self.write8(PRER_LO_REG, (prescale & 0xff) as u8);
        self.write8(PRER_HI_REG, ((prescale >> 8) & 0xff) as u8);
        self.write8(CTR_REG, self.read8(CTR_REG) | CTR_READY_MASK);
    }

    fn wait_if(&self) -> Result<(), Ls2kI2cError> {
        for _ in 0..XFER_TIMEOUT_LOOPS {
            if self.read8(SR_REG) & SR_IF != 0 {
                self.write8(CR_REG, CR_IACK);
                return Ok(());
            }
            delay_ns(1_000);
        }
        Err(Ls2kI2cError::Timeout)
    }

    /// 发起一次 8 位命令并等待完成，随后检查 AL/NOACK。
    fn command_status(&self, cmd: u8) -> Result<(), Ls2kI2cError> {
        self.write8(CR_REG, cmd);
        self.wait_if()?;
        let status = self.read8(SR_REG);
        if status & SR_AL != 0 {
            return Err(Ls2kI2cError::ArbitrationLost);
        }
        if status & SR_NOACK != 0 {
            return Err(Ls2kI2cError::NoAck);
        }
        Ok(())
    }

    /// 发送一个数据字节（已写 TXR）。
    fn send_byte(&self) -> Result<(), Ls2kI2cError> {
        self.command_status(CR_WRITE)
    }

    /// 发起 START + 地址（Linux ls2x_i2c_start：CR_START|CR_WRITE）。
    fn start(&self, address: u8, read: bool) -> Result<(), Ls2kI2cError> {
        self.write8(TXR_REG, (address << 1) | u8::from(read));
        self.command_status(CR_START | CR_WRITE)
    }

    fn stop(&self) -> Result<(), Ls2kI2cError> {
        self.write8(CR_REG, CR_STOP);
        for _ in 0..STOP_TIMEOUT_LOOPS {
            if self.read8(SR_REG) & SR_BUSY == 0 {
                return Ok(());
            }
            delay_ns(1_000);
        }
        Err(Ls2kI2cError::Timeout)
    }

    /// 向 `address` 写 `data`（无寄存器地址）。
    pub fn write(&self, address: u8, data: &[u8]) -> Result<(), Ls2kI2cError> {
        let _guard = self.lock.lock();
        self.start(address, false)?;
        for byte in data {
            self.write8(TXR_REG, *byte);
            self.send_byte()?;
        }
        self.stop()
    }

    /// 从 `address` 读 `len` 字节（无寄存器地址）。
    pub fn read(&self, address: u8, out: &mut [u8]) -> Result<(), Ls2kI2cError> {
        let _guard = self.lock.lock();
        self.start(address, true)?;
        for index in 0..out.len() {
            let last = index + 1 == out.len();
            // 与 Linux ls2x_i2c_rx 一致：最后字节的读命令带 ACK 位。
            self.write8(CR_REG, CR_READ | if last { CR_ACK } else { 0 });
            self.wait_if()?;
            out[index] = self.read8(RXR_REG);
        }
        self.stop()
    }

    /// 读寄存器：写寄存器地址后重发 START 读。
    pub fn read_regs(&self, address: u8, reg: u8, out: &mut [u8]) -> Result<(), Ls2kI2cError> {
        let _guard = self.lock.lock();
        self.start(address, false)?;
        self.write8(TXR_REG, reg);
        self.send_byte()?;
        self.start(address, true)?;
        for index in 0..out.len() {
            let last = index + 1 == out.len();
            self.write8(CR_REG, CR_READ | if last { CR_ACK } else { 0 });
            self.wait_if()?;
            out[index] = self.read8(RXR_REG);
        }
        self.stop()
    }

    /// 写寄存器。
    pub fn write_regs(&self, address: u8, reg: u8, data: &[u8]) -> Result<(), Ls2kI2cError> {
        let _guard = self.lock.lock();
        self.start(address, false)?;
        self.write8(TXR_REG, reg);
        self.send_byte()?;
        for byte in data {
            self.write8(TXR_REG, *byte);
            self.send_byte()?;
        }
        self.stop()
    }
}

// ─────────────────────────── 设备函数 ───────────────────────────

/// I2C 总线设备函数：操作契约 opcode 1=write、2=read、3=write_regs、
/// 4=read_regs（地址/寄存器/长度按扁平字节编码）。
pub struct Ls2xI2cFunction {
    bus: Arc<Ls2xI2cBus>,
    name: alloc::string::String,
    gone: core::sync::atomic::AtomicBool,
}

impl Ls2xI2cFunction {
    pub fn new(bus: Arc<Ls2xI2cBus>, name: alloc::string::String) -> Self {
        Self {
            bus,
            name,
            gone: core::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl DeviceFunction for Ls2xI2cFunction {
    fn class_id(&self) -> DeviceClassId {
        DeviceClassId::dynamic(0x6c73_326b_6932_6362) // "ls2k-i2c" 稳定哈希
    }

    fn dev_name(&self) -> &str {
        &self.name
    }

    fn operation_contract(&self) -> Option<&str> {
        Some("mygo.device.i2c-bus@1;1=write;2=read;3=write_regs;4=read_regs")
    }

    fn invoke(
        &self,
        opcode: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, DeviceFunctionInvokeError> {
        use general::dev::function::DeviceFunctionInvokeError as InvokeError;
        if self.gone.load(core::sync::atomic::Ordering::Acquire) {
            return Err(InvokeError::Gone);
        }
        match opcode {
            1 => {
                // input: [addr, data...]
                let Some((&address, data)) = input.split_first() else {
                    return Err(InvokeError::Invalid);
                };
                if !output.is_empty() {
                    return Err(InvokeError::Invalid);
                }
                self.bus.write(address, data).map_err(map_i2c_error)?;
                Ok(0)
            }
            2 => {
                // input: [addr, len]
                if input.len() != 2 {
                    return Err(InvokeError::Invalid);
                }
                let len = usize::from(input[1]);
                if output.len() < len {
                    return Err(InvokeError::Invalid);
                }
                self.bus
                    .read(input[0], &mut output[..len])
                    .map_err(map_i2c_error)?;
                Ok(len)
            }
            3 => {
                // input: [addr, reg, data...]
                if input.len() < 2 {
                    return Err(InvokeError::Invalid);
                }
                if !output.is_empty() {
                    return Err(InvokeError::Invalid);
                }
                self.bus
                    .write_regs(input[0], input[1], &input[2..])
                    .map_err(map_i2c_error)?;
                Ok(0)
            }
            4 => {
                // input: [addr, reg, len]
                if input.len() != 3 {
                    return Err(InvokeError::Invalid);
                }
                let len = usize::from(input[2]);
                if output.len() < len {
                    return Err(InvokeError::Invalid);
                }
                self.bus
                    .read_regs(input[0], input[1], &mut output[..len])
                    .map_err(map_i2c_error)?;
                Ok(len)
            }
            _ => Err(InvokeError::Unsupported),
        }
    }

    fn mark_gone(&self) {
        self.gone.store(true, core::sync::atomic::Ordering::Release);
    }

    fn is_gone(&self) -> bool {
        self.gone.load(core::sync::atomic::Ordering::Acquire)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn map_i2c_error(error: Ls2kI2cError) -> DeviceFunctionInvokeError {
    use general::dev::function::DeviceFunctionInvokeError as InvokeError;
    match error {
        Ls2kI2cError::Timeout => InvokeError::Busy,
        Ls2kI2cError::NoAck | Ls2kI2cError::ArbitrationLost => InvokeError::Fault,
        Ls2kI2cError::Invalid => InvokeError::Invalid,
    }
}

// ─────────────────────────── PnP 驱动 ───────────────────────────

struct Ls2kI2cBinding {
    bus: Arc<Ls2xI2cBus>,
}

pub struct Ls2kI2cDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl Ls2kI2cDriver {
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_LOONGSON_LS_I2C)
    }

    fn acquire_clock_hz(&self, info: &PlatformDeviceInfo) -> Result<u64, PnpError> {
        match info.acquire_dtb_resource_at(PROP_CLOCKS, 0) {
            Ok(clock) => {
                clock
                    .control(DtbResourceRequest::Enable)
                    .map_err(DtbProviderError::into_pnp_error)?;
                match clock
                    .control(DtbResourceRequest::GetRate)
                    .map_err(DtbProviderError::into_pnp_error)?
                {
                    DtbResourceReply::Value(hz) => Ok(hz),
                    _ => Err(PnpError::hardware_failure("i2c clock has no rate")),
                }
            }
            Err(DtbProviderError::Disabled | DtbProviderError::Invalid) => Ok(0),
            Err(error) => Err(error.into_pnp_error()),
        }
    }
}

impl PnpDriver for Ls2kI2cDriver {
    fn name(&self) -> &'static str {
        "platform-ls2k-i2c"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        if !matches!(id, PnpId::Platform { .. }) {
            return false;
        }
        info.as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = dev
            .info
            .as_any()
            .downcast_ref::<PlatformDeviceInfo>()
            .ok_or(PnpError::InvalidState)?;
        let Some((phys, size)) = info.first_mmio() else {
            return Err(PnpError::missing(PnpResourceKind::Mmio, "i2c reg missing"));
        };
        if size < 0x08 {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "i2c register window too small",
            ));
        }
        let clk_hz = self.acquire_clock_hz(info)?;
        let bus = Arc::new(Ls2xI2cBus::new((self.device_mmio_to_virt)(phys), clk_hz));
        let name = alloc::format!("i2c-{}", info.u32_property("bus_id").unwrap_or(0));
        let function: Arc<dyn DeviceFunction> =
            Arc::new(Ls2xI2cFunction::new(Arc::clone(&bus), name));
        dev.register_function(function)?;
        log::printk!(
            "[ls2k-i2c] bound {} phys={:#x} clk={} Hz",
            dev.id,
            phys,
            clk_hz
        );
        dev.set_driver_data(Arc::new(Ls2kI2cBinding { bus }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(_binding) = data.downcast::<Ls2kI2cBinding>()
        {}
        log::printk!("[ls2k-i2c] removed {}", dev.id);
    }
}

struct Ls2kI2cFactory;

impl DriverFactory for Ls2kI2cFactory {
    fn name(&self) -> &'static str {
        "platform-ls2k-i2c"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Ls2kI2cDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Ls2kI2cFactory))
}
