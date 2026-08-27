//! Loongson LS2K 温度传感器 platform ELM 驱动。
//!
//! 匹配工厂 DTB 的 `loongson,ls2k-tsensor` 节点（reg 0x1fe01500 窗口
//! 0x30，irq 7，thermal-zones/cpu-thermal 引用）。寄存器与温度换算对照
//! Linux drivers/thermal/loongson2_thermal.c：
//!
//! - THSENS_CTRL_HI（0x0）/ THSENS_CTRL_LOW（0x8）：高/低温阈值槽
//!   （writew(温度+100 | 使能 0x100)，sensor_sel=0 时槽偏移 0）；
//! - THSENS_STATUS（0x10）：写 INT_EN（0x3）清除中断；
//! - THSENS_OUT（0x14）：当前温度编码，温度 = (OUT & 0xFF - 100) °C。
//!
//! probe 时设置 60/95 °C 阈值并使能中断，注册 IRQ handler 在阈值越界
//! 时清除状态并记录温度；设备函数暴露
//! `mygo.device.thermal@1;1=read_temp_milli:i32` 供内核/用户态读取。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use general::dev::function::{DeviceClassId, DeviceFunction, DeviceFunctionInvokeError};
use general::dev::irq::{self, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use general::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use general::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDependency, PnpDevice,
    PnpDriver, PnpError, PnpId, PnpResourceKind, register_driver_factory,
};

const COMPAT_LOONGSON_LS2K_TSENSOR: &str = "loongson,ls2k-tsensor";

const THSENS_CTRL_HI_REG: usize = 0x0;
const THSENS_CTRL_LOW_REG: usize = 0x8;
const THSENS_STATUS_REG: usize = 0x10;
const THSENS_OUT_REG: usize = 0x14;
const MIN_REG_SIZE: usize = THSENS_OUT_REG + 4;

const INT_EN: u16 = 0x3;
const CTRL_ENABLE: u16 = 0x100;
const OUT_MASK: u32 = 0xff;
/// 编码零点是 100（摄氏度）。
const HECTO: i32 = 100;
const KILO: i32 = 1000;

/// 默认阈值：低温 60°C、高温 95°C。
const DEFAULT_LOW_C: i32 = 60;
const DEFAULT_HIGH_C: i32 = 95;

pub struct Ls2kTsensor {
    base: usize,
    active: AtomicBool,
    last_temp_milli: AtomicI32,
    threshold_crossings: AtomicUsize,
}

impl Ls2kTsensor {
    pub fn new(base: usize) -> Result<Self, &'static str> {
        let sensor = Self {
            base,
            active: AtomicBool::new(true),
            last_temp_milli: AtomicI32::new(0),
            threshold_crossings: AtomicUsize::new(0),
        };
        sensor.set_threshold(DEFAULT_LOW_C, DEFAULT_HIGH_C)?;
        sensor.clear_irq();
        Ok(sensor)
    }

    fn read32(&self, offset: usize) -> u32 {
        // Safety: offset 为固定寄存器偏移且窗口在 probe 校验，base 已映射。
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u32) }
    }

    fn write16(&self, offset: usize, value: u16) {
        // Safety: 同 read32，目标寄存器允许 16 位易失写入。
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u16, value) }
    }

    /// 设置高/低温阈值（摄氏度），使能越界中断。
    fn set_threshold(&self, low_c: i32, high_c: i32) -> Result<(), &'static str> {
        let low = (low_c + HECTO).clamp(0, 0x7fff) as u16 | CTRL_ENABLE;
        let high = (high_c + HECTO).clamp(0, 0x7fff) as u16 | CTRL_ENABLE;
        self.write16(THSENS_CTRL_LOW_REG, low);
        self.write16(THSENS_CTRL_HI_REG, high);
        Ok(())
    }

    fn clear_irq(&self) {
        self.write16(THSENS_STATUS_REG, INT_EN);
    }

    /// 读取当前温度（毫摄氏度）。
    pub fn read_temp_milli(&self) -> i32 {
        let raw = (self.read32(THSENS_OUT_REG) & OUT_MASK) as i32;
        let milli = (raw - HECTO) * KILO;
        self.last_temp_milli.store(milli, Ordering::Release);
        milli
    }

    /// IRQ 确认：清状态、采样温度并计数越界事件。
    pub fn acknowledge(&self) -> bool {
        self.clear_irq();
        self.read_temp_milli();
        self.threshold_crossings.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub fn mark_gone(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub fn last_temp_milli(&self) -> i32 {
        self.last_temp_milli.load(Ordering::Acquire)
    }

    pub fn threshold_crossings(&self) -> usize {
        self.threshold_crossings.load(Ordering::Relaxed)
    }
}

struct Ls2kTsensorIrqHandler {
    sensor: Arc<Ls2kTsensor>,
}

impl IrqHandler for Ls2kTsensorIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        if !self.sensor.is_active() {
            return IrqStatus::Unhandled;
        }
        self.sensor.acknowledge();
        log::printk!(
            "[ls2k-tsensor] thermal threshold crossed: temp={} m°C crossings={}",
            self.sensor.last_temp_milli(),
            self.sensor.threshold_crossings(),
        );
        IrqStatus::Handled
    }
}

/// 温度传感器设备函数。
pub struct Ls2kTsensorFunction {
    sensor: Arc<Ls2kTsensor>,
    name: alloc::string::String,
}

impl Ls2kTsensorFunction {
    pub fn new(sensor: Arc<Ls2kTsensor>, name: alloc::string::String) -> Self {
        Self { sensor, name }
    }
}

impl DeviceFunction for Ls2kTsensorFunction {
    fn class_id(&self) -> DeviceClassId {
        DeviceClassId::dynamic(0x6c73_326b_7473_6e73) // "ls2k-tsns" 稳定哈希
    }

    fn dev_name(&self) -> &str {
        &self.name
    }

    fn operation_contract(&self) -> Option<&str> {
        Some("mygo.device.thermal@1;1=read_temp_milli:i32")
    }

    fn invoke(
        &self,
        opcode: u32,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<usize, DeviceFunctionInvokeError> {
        use general::dev::function::DeviceFunctionInvokeError as InvokeError;
        if !self.sensor.is_active() {
            return Err(InvokeError::Gone);
        }
        match opcode {
            1 => {
                if !input.is_empty() || output.len() < 4 {
                    return Err(InvokeError::Invalid);
                }
                output[..4].copy_from_slice(&self.sensor.read_temp_milli().to_le_bytes());
                Ok(4)
            }
            _ => Err(InvokeError::Unsupported),
        }
    }

    fn mark_gone(&self) {
        self.sensor.mark_gone();
    }

    fn is_gone(&self) -> bool {
        !self.sensor.is_active()
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

struct Ls2kTsensorBinding {
    sensor: Arc<Ls2kTsensor>,
    irq_handle: IrqHandle,
}

pub struct Ls2kTsensorDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl Ls2kTsensorDriver {
    pub const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_LOONGSON_LS2K_TSENSOR)
    }
}

impl PnpDriver for Ls2kTsensorDriver {
    fn name(&self) -> &'static str {
        "platform-ls2k-tsensor"
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
            return Err(PnpError::missing(
                PnpResourceKind::Mmio,
                "tsensor reg missing",
            ));
        };
        if size < MIN_REG_SIZE {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "tsensor register window too small",
            ));
        }
        let sensor = Arc::new(
            Ls2kTsensor::new((self.device_mmio_to_virt)(phys))
                .map_err(|_| PnpError::hardware_failure("tsensor init failed"))?,
        );
        let temp_milli = sensor.read_temp_milli();
        let name = alloc::format!("tsensor-{}", info.u32_property("id").unwrap_or(0));
        let function: Arc<dyn DeviceFunction> =
            Arc::new(Ls2kTsensorFunction::new(Arc::clone(&sensor), name));
        dev.register_function(function)?;

        let handler: Arc<dyn IrqHandler> = Arc::new(Ls2kTsensorIrqHandler {
            sensor: Arc::clone(&sensor),
        });
        let irq_handle = match info.register_first_irq_handler(handler) {
            Ok(handle) => handle,
            Err(PlatformIrqRegistrationError::NoResource) => {
                return Err(PnpError::missing(
                    PnpResourceKind::Irq,
                    "tsensor irq missing",
                ));
            }
            Err(PlatformIrqRegistrationError::Unresolved) => {
                return Err(PnpError::dependency(
                    info.irq_resources()
                        .find_map(|irq| irq.controller())
                        .map(PnpDependency::IrqController)
                        .unwrap_or(PnpDependency::DefaultIrqDomain),
                ));
            }
            Err(PlatformIrqRegistrationError::RegistrationFailed { .. }) => {
                return Err(PnpError::hardware_failure(
                    "tsensor irq registration failed",
                ));
            }
        };
        log::printk!(
            "[ls2k-tsensor] bound {} phys={:#x} temp={} m°C thresholds={}..{}°C",
            dev.id,
            phys,
            temp_milli,
            DEFAULT_LOW_C,
            DEFAULT_HIGH_C,
        );
        dev.set_driver_data(Arc::new(Ls2kTsensorBinding { sensor, irq_handle }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<Ls2kTsensorBinding>()
        {
            binding.sensor.mark_gone();
            let _ = irq::unregister_irq_handler(binding.irq_handle);
        }
        log::printk!("[ls2k-tsensor] removed {}", dev.id);
    }
}

struct Ls2kTsensorFactory;

impl DriverFactory for Ls2kTsensorFactory {
    fn name(&self) -> &'static str {
        "platform-ls2k-tsensor"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Ls2kTsensorDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Ls2kTsensorFactory))
}
