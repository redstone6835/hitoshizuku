//! Google Goldfish RTC platform ELM 驱动。
//!
//! QEMU RISC-V virt 机器通过 `google,goldfish-rtc` DTB 节点提供真实墙钟时间。
//! 本驱动只声明 RTC class 已经稳定需要的读/写时间能力，并在 probe 时把硬件时间
//! 安装为内核 realtime source；alarm IRQ 暂不声明，避免未使用能力影响平台中断路径。

use alloc::sync::Arc;
use core::any::Any;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::dev::platform::PlatformDeviceInfo;
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDevice, PnpDriver,
    PnpError, PnpId, PnpResourceKind, RealtimeClockSource, register_driver_factory,
};
use crate::dev::rtc::{RtcDateTime, RtcDevice, RtcDriver, RtcError, RtcFeatures, RtcFunction};

const COMPAT_GOLDFISH_RTC: &str = "google,goldfish-rtc";

const RTC_TIME_LOW: usize = 0x00;
const RTC_TIME_HIGH: usize = 0x04;
const RTC_MIN_SIZE: usize = RTC_TIME_HIGH + core::mem::size_of::<u32>();
const NO_REALTIME_SOURCE: usize = 0;

fn realtime_source_id(phys: usize) -> usize {
    phys.checked_add(1).unwrap_or(usize::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GoldfishRtcError {
    RegisterWindowTooSmall,
    InvalidDate,
}

struct GoldfishRtc {
    base: usize,
    size: usize,
}

impl GoldfishRtc {
    const fn new(base: usize, size: usize) -> Self {
        Self { base, size }
    }

    fn ensure_register_window(&self) -> Result<(), GoldfishRtcError> {
        if self.size < RTC_MIN_SIZE {
            return Err(GoldfishRtcError::RegisterWindowTooSmall);
        }
        Ok(())
    }

    fn read32(&self, offset: usize) -> u32 {
        // Safety: probe 已验证 Goldfish RTC 窗口至少覆盖两个 32 位时间寄存器，
        // 本模块只会传入这两个对齐的固定偏移。
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write32(&self, offset: usize, value: u32) {
        // Safety: 安全条件与 `read32` 相同，目标寄存器允许 32 位易失写入。
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    fn read_unix_time_ns(&self) -> Result<u64, GoldfishRtcError> {
        self.ensure_register_window()?;
        // Goldfish RTC 要求先读 low；这会锁存对应 high，随后读 high 得到同一快照。
        let low = self.read32(RTC_TIME_LOW) as u64;
        let high = self.read32(RTC_TIME_HIGH) as u64;
        Ok((high << 32) | low)
    }

    fn read_datetime(&self) -> Result<RtcDateTime, GoldfishRtcError> {
        RtcDateTime::from_unix_time_ns(self.read_unix_time_ns()?)
            .ok_or(GoldfishRtcError::InvalidDate)
    }

    fn write_datetime(&self, time: RtcDateTime) -> Result<(), GoldfishRtcError> {
        self.ensure_register_window()?;
        let ns = time.unix_time_ns().ok_or(GoldfishRtcError::InvalidDate)?;
        // Linux goldfish RTC 驱动按 high -> low 写入；low 写入完成后 QEMU 更新 offset。
        self.write32(RTC_TIME_HIGH, (ns >> 32) as u32);
        self.write32(RTC_TIME_LOW, ns as u32);
        Ok(())
    }
}

impl RtcDriver for GoldfishRtc {
    fn read_time(&self) -> Result<RtcDateTime, RtcError> {
        self.read_datetime().map_err(map_goldfish_rtc_error)
    }

    fn set_time(&self, time: RtcDateTime) -> Result<(), RtcError> {
        self.write_datetime(time).map_err(map_goldfish_rtc_error)
    }

    fn features(&self) -> RtcFeatures {
        RtcFeatures::READ_TIME.with(RtcFeatures::SET_TIME)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn map_goldfish_rtc_error(err: GoldfishRtcError) -> RtcError {
    match err {
        GoldfishRtcError::RegisterWindowTooSmall => RtcError::Io,
        GoldfishRtcError::InvalidDate => RtcError::Invalid,
    }
}

struct GoldfishRtcBinding {
    rtc_dev: Arc<RtcDevice>,
}

pub struct GoldfishRtcPlatformDriver {
    device_mmio_to_virt: fn(usize) -> usize,
    set_realtime_ns: Option<fn(u64)>,
    install_realtime_source: Option<fn(RealtimeClockSource) -> bool>,
    unregister_realtime_source: Option<fn(usize)>,
    realtime_owner: AtomicUsize,
}

impl GoldfishRtcPlatformDriver {
    const fn new(
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
        info.has_id(COMPAT_GOLDFISH_RTC)
    }

    fn install_realtime_clock(&self, dev: &PnpDevice, phys: usize, realtime_ns: u64) {
        let source_id = realtime_source_id(phys);
        if let Some(install) = self.install_realtime_source {
            let source = RealtimeClockSource {
                id: source_id,
                name: "platform-goldfish-rtc",
                realtime_ns,
            };
            if install(source) {
                self.realtime_owner.store(source_id, Ordering::Release);
                log::printk!(
                    "[platform-goldfish-rtc] installed realtime source from {} phys={:#x} unix_ns={}",
                    dev.id,
                    phys,
                    realtime_ns
                );
            } else {
                log::printk!(
                    "[platform-goldfish-rtc] realtime source from {} phys={:#x} ignored: another RTC owns realtime",
                    dev.id,
                    phys
                );
            }
            return;
        }

        if let Some(set_realtime_ns) = self.set_realtime_ns {
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
                    "[platform-goldfish-rtc] installed legacy realtime clock from {} phys={:#x} unix_ns={}",
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
                "[platform-goldfish-rtc] unregistered realtime source from {} phys={:#x}",
                dev.id,
                phys
            );
        }
    }
}

impl PnpDriver for GoldfishRtcPlatformDriver {
    fn name(&self) -> &'static str {
        "platform-goldfish-rtc"
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
        let (phys, size) = info.first_mmio().ok_or(PnpError::missing(
            PnpResourceKind::Mmio,
            "goldfish rtc reg missing",
        ))?;
        if size < RTC_MIN_SIZE {
            return Err(PnpError::malformed(
                PnpResourceKind::Mmio,
                "goldfish rtc reg window too small",
            ));
        }

        let rtc = Arc::new(GoldfishRtc::new((self.device_mmio_to_virt)(phys), size));
        let realtime_ns = rtc.read_unix_time_ns().map_err(|err| {
            log::printk!(
                "[platform-goldfish-rtc] probe failed for {} phys={:#x}: {:?}",
                dev.id,
                phys,
                err
            );
            PnpError::hardware_failure("rtc initial time read failed")
        })?;

        let rtc_driver: Arc<dyn RtcDriver> = rtc;
        let rtc_projection_name = RtcDevice::alloc_stable_projection_name(&dev.name)?;
        let rtc_dev = Arc::new(RtcDevice::new(rtc_projection_name, rtc_driver));
        dev.register_function(RtcFunction::new_arc(Arc::clone(&rtc_dev)))?;
        self.install_realtime_clock(dev, phys, realtime_ns);
        dev.set_driver_data(Arc::new(GoldfishRtcBinding { rtc_dev }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(info) = dev.info.as_any().downcast_ref::<PlatformDeviceInfo>()
            && let Some((phys, _)) = info.first_mmio()
        {
            self.unregister_realtime_clock(dev, phys);
        }
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<GoldfishRtcBinding>()
        {
            binding.rtc_dev.mark_gone();
        }
        log::printk!("[platform-goldfish-rtc] removed {}", dev.id);
    }
}

struct GoldfishRtcFactory;

impl DriverFactory for GoldfishRtcFactory {
    fn name(&self) -> &'static str {
        "platform-goldfish-rtc"
    }

    fn create(&self, ctx: &DevInitContext) -> Result<Arc<dyn PnpDriver>, PnpError> {
        Ok(Arc::new(GoldfishRtcPlatformDriver::new(
            ctx.device_mmio_to_virt,
            ctx.set_realtime_ns,
            ctx.install_realtime_source,
            ctx.unregister_realtime_source,
        )))
    }
}

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(GoldfishRtcFactory))
}
