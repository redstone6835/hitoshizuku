//! Loongson LS7A RTC platform 驱动。
//!
//! 固件层只负责把 `compatible` 与 MMIO resource 注册成 platform 设备。本模块
//! 负责匹配 `loongson,ls7a-rtc`、访问 LS7A RTC 寄存器，并把读到的硬件时间交给
//! 内核 realtime 时钟回调。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, PnpBusInfo, PnpDevice, PnpDriver, PnpError, PnpId,
    RealtimeClockSource, register_driver_factory,
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
const NO_REALTIME_SOURCE: usize = 0;

/// LS7A TOY year 寄存器存储的是从 1900 开始的年偏移。
const TOY_YEAR_BASE: u32 = 1900;
/// 跨秒边界读取时 year/read0 可能不一致，重试几次再判定不稳定。
const TOY_STABLE_READ_RETRIES: usize = 3;
const TOY_SECOND_SHIFT: u32 = 4;
const TOY_SECOND_MASK: u32 = 0x3f;
const TOY_MINUTE_SHIFT: u32 = 10;
const TOY_MINUTE_MASK: u32 = 0x3f;
const TOY_HOUR_SHIFT: u32 = 16;
const TOY_HOUR_MASK: u32 = 0x1f;
const TOY_DAY_SHIFT: u32 = 21;
const TOY_DAY_MASK: u32 = 0x1f;
const TOY_MONTH_SHIFT: u32 = 26;
const TOY_MONTH_MASK: u32 = 0x3f;

fn realtime_source_id(phys: usize) -> usize {
    // MMIO 基址正常是对齐地址；+1 只用于避开 0 这个“无 owner”哨兵。
    // 极端溢出时保留 usize::MAX，仍然是非 0 的本次启动内标识。
    phys.checked_add(1).unwrap_or(usize::MAX)
}

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

        for _ in 0..TOY_STABLE_READ_RETRIES {
            let year0 = self.read32(TOY_READ1_REG)?;
            let read0 = self.read32(TOY_READ0_REG)?;
            let year1 = self.read32(TOY_READ1_REG)?;
            if year0 != year1 {
                continue;
            }

            let second = (read0 >> TOY_SECOND_SHIFT) & TOY_SECOND_MASK;
            let minute = (read0 >> TOY_MINUTE_SHIFT) & TOY_MINUTE_MASK;
            let hour = (read0 >> TOY_HOUR_SHIFT) & TOY_HOUR_MASK;
            let day = (read0 >> TOY_DAY_SHIFT) & TOY_DAY_MASK;
            let month = (read0 >> TOY_MONTH_SHIFT) & TOY_MONTH_MASK;
            let year = year0
                .checked_add(TOY_YEAR_BASE)
                .ok_or(Ls7aRtcError::Overflow)?;
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
    install_realtime_source: Option<fn(RealtimeClockSource) -> bool>,
    unregister_realtime_source: Option<fn(usize)>,
    realtime_owner: AtomicUsize,
}

impl Ls7aRtcPlatformDriver {
    pub const fn new(
        device_mmio_to_virt: fn(usize) -> usize,
        set_realtime_ns: Option<fn(u64)>,
        install_realtime_source: Option<fn(RealtimeClockSource) -> bool>,
        unregister_realtime_source: Option<fn(usize)>,
    ) -> Self {
        Self {
            device_mmio_to_virt,
            set_realtime_ns,
            install_realtime_source,
            unregister_realtime_source,
            realtime_owner: AtomicUsize::new(NO_REALTIME_SOURCE),
        }
    }

    fn matches_platform(info: &PlatformDeviceInfo) -> bool {
        info.has_id(COMPAT_LOONGSON_LS7A_RTC)
    }

    fn install_realtime_clock(&self, dev: &PnpDevice, phys: usize, realtime_ns: u64) {
        let source_id = realtime_source_id(phys);
        if let Some(install) = self.install_realtime_source {
            let source = RealtimeClockSource {
                id: source_id,
                name: "platform-ls7a-rtc",
                realtime_ns,
            };
            if install(source) {
                self.realtime_owner.store(source_id, Ordering::Release);
                log::printk!(
                    "[platform-ls7a-rtc] installed realtime source from {} phys={:#x} unix_ns={}",
                    dev.id,
                    phys,
                    realtime_ns
                );
            } else {
                log::printk!(
                    "[platform-ls7a-rtc] realtime source from {} phys={:#x} ignored: another RTC owns realtime",
                    dev.id,
                    phys
                );
            }
            return;
        }

        if let Some(set_realtime_ns) = self.set_realtime_ns {
            // 兼容旧 hook：没有 unregister 回调时，只在驱动本地记录 owner。
            // remove 时清掉该标记，允许同类或替代 RTC 后续重新设置 realtime。
            let installed = self
                .realtime_owner
                .compare_exchange(
                    NO_REALTIME_SOURCE,
                    source_id,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
                || self.realtime_owner.load(Ordering::Acquire) == source_id;
            if installed {
                set_realtime_ns(realtime_ns);
                log::printk!(
                    "[platform-ls7a-rtc] installed legacy realtime clock from {} phys={:#x} unix_ns={}",
                    dev.id,
                    phys,
                    realtime_ns
                );
            }
        }
    }

    fn unregister_realtime_clock(&self, dev: &PnpDevice, phys: usize) {
        let source_id = realtime_source_id(phys);
        if self.realtime_owner.load(Ordering::Acquire) != source_id {
            return;
        }

        if let Some(unregister) = self.unregister_realtime_source {
            unregister(source_id);
        }
        if self
            .realtime_owner
            .compare_exchange(
                source_id,
                NO_REALTIME_SOURCE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            log::printk!(
                "[platform-ls7a-rtc] unregistered realtime source from {} phys={:#x}",
                dev.id,
                phys
            );
        }
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

        self.install_realtime_clock(dev, phys, realtime_ns);

        dev.set_driver_data(rtc);
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(info) = dev.info.as_any().downcast_ref::<PlatformDeviceInfo>()
            && let Some((phys, _)) = info.first_mmio()
        {
            self.unregister_realtime_clock(dev, phys);
        }
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
            ctx.install_realtime_source,
            ctx.unregister_realtime_source,
        )))
    }
}

pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_driver_factory(Arc::new(Ls7aRtcFactory)).map(|_| ())
}
