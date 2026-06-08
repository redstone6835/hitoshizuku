//! DTB syscon 与 syscon power 功能节点驱动。
//!
//! `syscon` 只登记共享寄存器块；`syscon-poweroff`/`syscon-reboot` 通过 `regmap`
//! 绑定到该寄存器块，并把最终动作安装到统一 power 控制接口。整个路径使用
//! phandle 和 typed register 描述，不把底层设备身份投影成 POSIX 设备号。

use alloc::sync::Arc;
use core::ptr::{read_volatile, write_volatile};

use crate::dev::platform::{FirmwarePropertyValue, PlatformDeviceInfo};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    register_driver_factory,
};
use crate::dev::syscon::{self, SysconAccessWidth, SysconDevice, SysconError, SysconHandle};
use crate::firmware::power::{
    PowerAccessWidth, PowerControlMethod, PowerRegister, PowerRegisterSpace,
};

const COMPAT_SYSCON: &str = "syscon";
const COMPAT_SYSCON_POWEROFF: &str = "syscon-poweroff";
const COMPAT_SYSCON_REBOOT: &str = "syscon-reboot";

const PROP_REG_IO_WIDTH: &str = "reg-io-width";
const PROP_REG_SHIFT: &str = "reg-shift";
const PROP_REGMAP: &str = "regmap";
const PROP_OFFSET: &str = "offset";
const PROP_VALUE: &str = "value";

struct MmioSyscon {
    phandle: u32,
    phys: usize,
    size: usize,
    base: usize,
    reg_shift: u8,
    width: SysconAccessWidth,
}

impl MmioSyscon {
    fn new(
        phandle: u32,
        phys: usize,
        size: usize,
        base: usize,
        reg_shift: u8,
        width: SysconAccessWidth,
    ) -> Self {
        Self {
            phandle,
            phys,
            size,
            base,
            reg_shift,
            width,
        }
    }

    fn resolve_offset(
        &self,
        offset: usize,
        width: SysconAccessWidth,
    ) -> Result<usize, SysconError> {
        let byte_offset = offset
            .checked_shl(u32::from(self.reg_shift))
            .ok_or(SysconError::OutOfRange)?;
        let end = byte_offset
            .checked_add(width.bytes())
            .ok_or(SysconError::OutOfRange)?;
        if end > self.size || byte_offset % width.bytes() != 0 {
            return Err(SysconError::OutOfRange);
        }
        Ok(byte_offset)
    }
}

impl SysconDevice for MmioSyscon {
    fn phandle(&self) -> u32 {
        self.phandle
    }

    fn phys_range(&self) -> (usize, usize) {
        (self.phys, self.size)
    }

    fn default_width(&self) -> SysconAccessWidth {
        self.width
    }

    fn phys_addr_for(&self, offset: usize, width: SysconAccessWidth) -> Option<usize> {
        let byte_offset = self.resolve_offset(offset, width).ok()?;
        self.phys.checked_add(byte_offset)
    }

    fn read(&self, offset: usize, width: SysconAccessWidth) -> Result<u64, SysconError> {
        let byte_offset = self.resolve_offset(offset, width)?;
        let addr = self
            .base
            .checked_add(byte_offset)
            .ok_or(SysconError::OutOfRange)?;
        let value = unsafe {
            match width {
                SysconAccessWidth::U8 => read_volatile(addr as *const u8) as u64,
                SysconAccessWidth::U16 => read_volatile(addr as *const u16) as u64,
                SysconAccessWidth::U32 => read_volatile(addr as *const u32) as u64,
                SysconAccessWidth::U64 => read_volatile(addr as *const u64),
            }
        };
        Ok(value)
    }

    fn write(
        &self,
        offset: usize,
        width: SysconAccessWidth,
        value: u64,
    ) -> Result<(), SysconError> {
        let byte_offset = self.resolve_offset(offset, width)?;
        let addr = self
            .base
            .checked_add(byte_offset)
            .ok_or(SysconError::OutOfRange)?;
        unsafe {
            match width {
                SysconAccessWidth::U8 => write_volatile(addr as *mut u8, value as u8),
                SysconAccessWidth::U16 => write_volatile(addr as *mut u16, value as u16),
                SysconAccessWidth::U32 => write_volatile(addr as *mut u32, value as u32),
                SysconAccessWidth::U64 => write_volatile(addr as *mut u64, value),
            }
        }
        Ok(())
    }
}

struct SysconBinding {
    handle: SysconHandle,
}

pub struct SysconPlatformDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl SysconPlatformDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_SYSCON)
    }
}

impl PnpDriver for SysconPlatformDriver {
    fn name(&self) -> &'static str {
        "platform-syscon"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .is_some_and(Self::matches_platform)
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = platform_info(dev)?;
        let phandle = info.properties.fw_phandle.ok_or(PnpError::ProbeFailed)?;
        let (phys, size) = info.first_mmio().ok_or(PnpError::ProbeFailed)?;
        let width = syscon_width(info).ok_or(PnpError::ProbeFailed)?;
        let reg_shift = u8::try_from(info.u32_property(PROP_REG_SHIFT).unwrap_or(0))
            .map_err(|_| PnpError::ProbeFailed)?;
        let syscon = Arc::new(MmioSyscon::new(
            phandle,
            phys,
            size,
            (self.device_mmio_to_virt)(phys),
            reg_shift,
            width,
        ));
        let handle = syscon::register(syscon).map_err(map_syscon_error)?;
        dev.set_driver_data(Arc::new(SysconBinding { handle }));
        log::printk!(
            "[syscon] registered {} phandle={:#x} phys={:#x} size={:#x} width={} shift={}",
            dev.name.as_ref(),
            phandle,
            phys,
            size,
            width.bytes(),
            reg_shift
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<SysconBinding>()
        {
            let _ = syscon::unregister(binding.handle);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SysconPowerAction {
    Shutdown,
    Reboot,
}

struct SysconPowerBinding {
    action: SysconPowerAction,
    regmap: u32,
    offset: usize,
    width: SysconAccessWidth,
    value: u64,
}

pub struct SysconPowerDriver {
    device_mmio_to_virt: fn(usize) -> usize,
}

impl SysconPowerDriver {
    const fn new(device_mmio_to_virt: fn(usize) -> usize) -> Self {
        Self {
            device_mmio_to_virt,
        }
    }

    fn action(info: &PlatformDeviceInfo) -> Option<SysconPowerAction> {
        if info.has_id(COMPAT_SYSCON_POWEROFF) {
            Some(SysconPowerAction::Shutdown)
        } else if info.has_id(COMPAT_SYSCON_REBOOT) {
            Some(SysconPowerAction::Reboot)
        } else {
            None
        }
    }
}

impl PnpDriver for SysconPowerDriver {
    fn name(&self) -> &'static str {
        "platform-syscon-power"
    }

    fn bus_type(&self) -> BusType {
        BusType::PLATFORM
    }

    fn matches(&self, id: &PnpId, info: &dyn PnpBusInfo) -> bool {
        matches!(id, PnpId::Platform { .. })
            && info
                .as_any()
                .downcast_ref::<PlatformDeviceInfo>()
                .and_then(Self::action)
                .is_some()
    }

    fn probe(&self, dev: &Arc<PnpDevice>) -> Result<(), PnpError> {
        let info = platform_info(dev)?;
        let action = Self::action(info).ok_or(PnpError::ProbeFailed)?;
        let regmap = info
            .u32_property(PROP_REGMAP)
            .ok_or(PnpError::ProbeFailed)?;
        let syscon = syscon::get(regmap).ok_or(PnpError::ProbeDeferred)?;
        let offset = usize_property(info, PROP_OFFSET).ok_or(PnpError::ProbeFailed)?;
        let value = u64_property(info, PROP_VALUE).ok_or(PnpError::ProbeFailed)?;
        let width = syscon.default_width();
        let phys = syscon
            .phys_addr_for(offset, width)
            .ok_or(PnpError::ProbeFailed)?;
        let access_width = power_width(width).ok_or(PnpError::ProbeFailed)?;
        let method = PowerControlMethod::RegisterWrite {
            register: PowerRegister {
                space: PowerRegisterSpace::SystemMemory,
                address: phys,
                access_width,
            },
            value,
        };

        match action {
            SysconPowerAction::Shutdown => {
                crate::firmware::power::install_shutdown(method, self.device_mmio_to_virt)
            }
            SysconPowerAction::Reboot => {
                crate::firmware::power::install_reboot(method, self.device_mmio_to_virt)
            }
        }

        dev.set_driver_data(Arc::new(SysconPowerBinding {
            action,
            regmap,
            offset,
            width,
            value,
        }));
        log::printk!(
            "[syscon-power] registered {:?} via regmap={:#x} offset={:#x} width={} value={:#x}",
            action,
            regmap,
            offset,
            width.bytes(),
            value
        );
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<SysconPowerBinding>()
        {
            log::debug!(
                "[syscon-power] removed {:?} regmap={:#x} offset={:#x} width={} value={:#x}",
                binding.action,
                binding.regmap,
                binding.offset,
                binding.width.bytes(),
                binding.value
            );
        }
    }
}

fn platform_info(dev: &Arc<PnpDevice>) -> Result<&PlatformDeviceInfo, PnpError> {
    dev.info
        .as_any()
        .downcast_ref::<PlatformDeviceInfo>()
        .ok_or(PnpError::InvalidState)
}

fn syscon_width(info: &PlatformDeviceInfo) -> Option<SysconAccessWidth> {
    let width = info.u32_property(PROP_REG_IO_WIDTH).unwrap_or(4) as usize;
    SysconAccessWidth::from_bytes(width)
}

fn power_width(width: SysconAccessWidth) -> Option<PowerAccessWidth> {
    PowerAccessWidth::from_bytes(width.bytes())
}

fn usize_property(info: &PlatformDeviceInfo, name: &str) -> Option<usize> {
    let value = u64_property(info, name)?;
    usize::try_from(value).ok()
}

fn u64_property(info: &PlatformDeviceInfo, name: &str) -> Option<u64> {
    info.fw_properties
        .iter()
        .find(|property| property.name.as_ref() == name)
        .and_then(|property| match &property.value {
            FirmwarePropertyValue::U32(value) => Some(u64::from(*value)),
            FirmwarePropertyValue::U32List(values) if values.len() == 1 => {
                Some(u64::from(values[0]))
            }
            FirmwarePropertyValue::U32List(values) if values.len() == 2 => {
                Some((u64::from(values[0]) << 32) | u64::from(values[1]))
            }
            FirmwarePropertyValue::Bool
            | FirmwarePropertyValue::U32List(_)
            | FirmwarePropertyValue::StringList(_)
            | FirmwarePropertyValue::Bytes(_) => None,
        })
}

fn map_syscon_error(err: SysconError) -> PnpError {
    match err {
        SysconError::AlreadyRegistered => PnpError::NameConflict,
        SysconError::Invalid | SysconError::OutOfRange => PnpError::ProbeFailed,
        SysconError::NotFound => PnpError::ProbeDeferred,
    }
}

struct SysconFactory;

impl DriverFactory for SysconFactory {
    fn name(&self) -> &'static str {
        "platform-syscon"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(SysconPlatformDriver::new(ctx.device_mmio_to_virt)))
    }
}

struct SysconPowerFactory;

impl DriverFactory for SysconPowerFactory {
    fn name(&self) -> &'static str {
        "platform-syscon-power"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(SysconPowerDriver::new(ctx.device_mmio_to_virt)))
    }
}

pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(SysconFactory))?;
    register_driver_factory(Arc::new(SysconPowerFactory)).map(|_| ())
}
