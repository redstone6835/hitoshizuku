//! Loongson LS7A RTC platform 驱动。
//!
//! 固件层只负责把 `compatible` 与 MMIO resource 注册成 platform 设备。本模块
//! 负责匹配 `loongson,ls7a-rtc`、访问 LS7A RTC 寄存器，并把读到的硬件时间交给
//! 内核 realtime 时钟回调。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    register_driver_factory,
};
use crate::dev::rtc::RtcDateTime;

const COMPAT_LOONGSON_LS7A_RTC: &str = "loongson,ls7a-rtc";

const TOY_READ0_REG: usize = 0x2c;
const TOY_READ1_REG: usize = 0x30;
const CTRL_REG: usize = 0x40;
const MIN_REG_SIZE: usize = CTRL_REG + core::mem::size_of::<u32>();

const CTRL_OSC_ENABLE: u32 = 1 << 8;
const CTRL_TOY_ENABLE: u32 = 1 << 11;
const CTRL_REQUIRED: u32 = CTRL_OSC_ENABLE | CTRL_TOY_ENABLE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ls7aRtcError {
    RegisterWindowTooSmall,
    CounterDisabled,
    UnstableRead,
    InvalidDate,
    Overflow,
}

pub struct Ls7aRtc {
    base: usize,
    size: usize,
}

impl Ls7aRtc {
    pub const fn new(base: usize, size: usize) -> Self {
        Self { base, size }
    }

    pub fn read_unix_time_ns(&self) -> Result<u64, Ls7aRtcError> {
        self.ensure_register_window()?;
        self.enable_counter()?;

        for _ in 0..3 {
            let year0 = self.read32(TOY_READ1_REG)?;
            let read0 = self.read32(TOY_READ0_REG)?;
            let year1 = self.read32(TOY_READ1_REG)?;
            if year0 != year1 {
                continue;
            }

            let second = (read0 >> 4) & 0x3f;
            let minute = (read0 >> 10) & 0x3f;
            let hour = (read0 >> 16) & 0x1f;
            let day = (read0 >> 21) & 0x1f;
            let month = (read0 >> 26) & 0x3f;
            let year = year0.checked_add(1900).ok_or(Ls7aRtcError::Overflow)?;
            return RtcDateTime::new(year, month, day, hour, minute, second)
                .and_then(RtcDateTime::unix_time_ns)
                .ok_or(Ls7aRtcError::InvalidDate);
        }

        Err(Ls7aRtcError::UnstableRead)
    }

    fn ensure_register_window(&self) -> Result<(), Ls7aRtcError> {
        if self.size != 0 && self.size < MIN_REG_SIZE {
            Err(Ls7aRtcError::RegisterWindowTooSmall)
        } else {
            Ok(())
        }
    }

    fn enable_counter(&self) -> Result<(), Ls7aRtcError> {
        let mut ctrl = self.read32(CTRL_REG)?;
        if ctrl & CTRL_REQUIRED == CTRL_REQUIRED {
            return Ok(());
        }

        ctrl |= CTRL_REQUIRED;
        self.write32(CTRL_REG, ctrl)?;
        let ctrl = self.read32(CTRL_REG)?;
        if ctrl & CTRL_REQUIRED == CTRL_REQUIRED {
            Ok(())
        } else {
            Err(Ls7aRtcError::CounterDisabled)
        }
    }

    fn read32(&self, offset: usize) -> Result<u32, Ls7aRtcError> {
        let addr = self
            .base
            .checked_add(offset)
            .ok_or(Ls7aRtcError::Overflow)?;
        Ok(unsafe { core::ptr::read_volatile(addr as *const u32) })
    }

    fn write32(&self, offset: usize, value: u32) -> Result<(), Ls7aRtcError> {
        let addr = self
            .base
            .checked_add(offset)
            .ok_or(Ls7aRtcError::Overflow)?;
        unsafe { core::ptr::write_volatile(addr as *mut u32, value) };
        Ok(())
    }
}

pub struct Ls7aRtcPlatformDriver {
    device_mmio_to_virt: fn(usize) -> usize,
    set_realtime_ns: Option<fn(u64)>,
    clock_installed: AtomicBool,
}

impl Ls7aRtcPlatformDriver {
    pub const fn new(
        device_mmio_to_virt: fn(usize) -> usize,
        set_realtime_ns: Option<fn(u64)>,
    ) -> Self {
        Self {
            device_mmio_to_virt,
            set_realtime_ns,
            clock_installed: AtomicBool::new(false),
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_LOONGSON_LS7A_RTC)
    }
}

impl PnpDriver for Ls7aRtcPlatformDriver {
    fn name(&self) -> &'static str {
        "platform-ls7a-rtc"
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
            return Err(PnpError::ProbeFailed);
        };

        let rtc = Arc::new(Ls7aRtc::new((self.device_mmio_to_virt)(phys), size));
        let realtime_ns = rtc.read_unix_time_ns().map_err(|err| {
            log::printk!(
                "[platform-ls7a-rtc] probe failed for {} phys={:#x}: {:?}",
                dev.id,
                phys,
                err
            );
            PnpError::ProbeFailed
        })?;

        if let Some(set_realtime_ns) = self.set_realtime_ns
            && !self.clock_installed.swap(true, Ordering::AcqRel)
        {
            set_realtime_ns(realtime_ns);
            log::printk!(
                "[platform-ls7a-rtc] installed realtime clock from {} phys={:#x} unix_ns={}",
                dev.id,
                phys,
                realtime_ns
            );
        }

        dev.set_driver_data(rtc);
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        let _ = dev.take_driver_data();
        log::printk!("[platform-ls7a-rtc] removed {}", dev.id);
    }
}

struct Ls7aRtcFactory;

impl DriverFactory for Ls7aRtcFactory {
    fn name(&self) -> &'static str {
        "platform-ls7a-rtc"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(Ls7aRtcPlatformDriver::new(
            ctx.device_mmio_to_virt,
            ctx.set_realtime_ns,
        )))
    }
}

pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(Ls7aRtcFactory)).map(|_| ())
}
