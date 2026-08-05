//! Google Goldfish RTC platform ELM 驱动。
//!
//! QEMU RISC-V virt 机器通过 `google,goldfish-rtc` DTB 节点提供真实墙钟时间。
//! 本驱动实现时间读写、alarm 编程和固件 IRQ 资源接入，并在 probe 时把硬件时间
//! 安装为内核 realtime source。IRQ 事件统一交给 RTC class 聚合，不在驱动中解释
//! `/dev/rtc*` 的 ioctl 或阻塞读取语义。

use alloc::sync::{Arc, Weak};
use core::any::Any;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::dev::irq::{self, IrqHandle, IrqHandler, IrqLine, IrqStatus};
use crate::dev::platform::{PlatformDeviceInfo, PlatformIrqRegistrationError};
use crate::dev::pnp::{
    BusType, DevInitContext, DriverFactory, DriverHandle, PnpBusInfo, PnpDependency, PnpDevice,
    PnpDriver, PnpError, PnpId, PnpResourceKind, RealtimeClockSource, register_driver_factory,
};
use crate::dev::rtc::{
    RtcAlarm, RtcDateTime, RtcDevice, RtcDriver, RtcError, RtcFeatures, RtcFunction, RtcIrqData,
    RtcIrqFlags,
};

const COMPAT_GOLDFISH_RTC: &str = "google,goldfish-rtc";

const RTC_TIME_LOW: usize = 0x00;
const RTC_TIME_HIGH: usize = 0x04;
const RTC_ALARM_LOW: usize = 0x08;
const RTC_ALARM_HIGH: usize = 0x0c;
const RTC_IRQ_ENABLED: usize = 0x10;
const RTC_CLEAR_ALARM: usize = 0x14;
const RTC_ALARM_STATUS: usize = 0x18;
const RTC_CLEAR_INTERRUPT: usize = 0x1c;
const RTC_MIN_SIZE: usize = RTC_TIME_HIGH + core::mem::size_of::<u32>();
const RTC_ALARM_MIN_SIZE: usize = RTC_CLEAR_INTERRUPT + core::mem::size_of::<u32>();
const NO_REALTIME_SOURCE: usize = 0;

fn realtime_source_id(phys: usize) -> usize {
    phys.checked_add(1).unwrap_or(usize::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GoldfishRtcError {
    RegisterWindowTooSmall,
    AlarmUnsupported,
    InvalidDate,
}

struct GoldfishRtc {
    base: usize,
    size: usize,
    alarm_irq_available: AtomicBool,
}

impl GoldfishRtc {
    const fn new(base: usize, size: usize) -> Self {
        Self {
            base,
            size,
            alarm_irq_available: AtomicBool::new(false),
        }
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

    fn ensure_alarm_register_window(&self) -> Result<(), GoldfishRtcError> {
        if self.size < RTC_ALARM_MIN_SIZE {
            return Err(GoldfishRtcError::AlarmUnsupported);
        }
        Ok(())
    }

    fn read_alarm_datetime(&self) -> Result<RtcAlarm, GoldfishRtcError> {
        self.ensure_alarm_register_window()?;
        // 与时间寄存器相同，先读 low 会锁存对应 high。
        let low = self.read32(RTC_ALARM_LOW) as u64;
        let high = self.read32(RTC_ALARM_HIGH) as u64;
        let time = RtcDateTime::from_unix_time_ns((high << 32) | low)
            .ok_or(GoldfishRtcError::InvalidDate)?;
        Ok(RtcAlarm {
            time,
            enabled: self.read32(RTC_ALARM_STATUS) != 0,
            pending: false,
        })
    }

    fn write_alarm_datetime(&self, alarm: RtcAlarm) -> Result<(), GoldfishRtcError> {
        self.ensure_alarm_register_window()?;
        if alarm.enabled {
            let ns = alarm
                .time
                .unix_time_ns()
                .ok_or(GoldfishRtcError::InvalidDate)?;
            // QEMU Goldfish RTC 按 high -> low 提交新的 alarm 时间；low 写入同时
            // 激活比较器，最后再打开 IRQ 门控。
            self.write32(RTC_ALARM_HIGH, (ns >> 32) as u32);
            self.write32(RTC_ALARM_LOW, ns as u32);
            self.write32(RTC_IRQ_ENABLED, 1);
        } else {
            self.write32(RTC_IRQ_ENABLED, 0);
            if self.read32(RTC_ALARM_STATUS) != 0 {
                self.write32(RTC_CLEAR_ALARM, 1);
            }
            // CLEAR_ALARM 停止 comparator；已经锁存到 IRQ 线的 pending 状态需要
            // 通过独立寄存器确认，避免下一次启用 alarm 时继承旧事件。
            self.write32(RTC_CLEAR_INTERRUPT, 1);
        }
        Ok(())
    }

    fn set_alarm_enabled(&self, enabled: bool) -> Result<(), GoldfishRtcError> {
        self.ensure_alarm_register_window()?;
        self.write32(RTC_IRQ_ENABLED, u32::from(enabled));
        if !enabled {
            // 关闭门控不会自动撤销已经锁存到 IRQ 线的事件；显式确认旧中断，
            // 避免稍后重新启用时立即上报一个过期 alarm。
            self.write32(RTC_CLEAR_INTERRUPT, 1);
        }
        Ok(())
    }

    fn clear_alarm(&self) -> Result<(), GoldfishRtcError> {
        self.ensure_alarm_register_window()?;
        self.write32(RTC_IRQ_ENABLED, 0);
        self.write32(RTC_CLEAR_ALARM, 1);
        self.write32(RTC_CLEAR_INTERRUPT, 1);
        Ok(())
    }

    fn acknowledge_alarm_irq(&self) -> Result<(), GoldfishRtcError> {
        self.ensure_alarm_register_window()?;
        self.write32(RTC_CLEAR_INTERRUPT, 1);
        Ok(())
    }

    fn set_alarm_irq_available(&self, available: bool) {
        self.alarm_irq_available.store(available, Ordering::Release);
    }

    fn alarm_irq_available(&self) -> bool {
        self.alarm_irq_available.load(Ordering::Acquire)
    }
}

impl RtcDriver for GoldfishRtc {
    fn read_time(&self) -> Result<RtcDateTime, RtcError> {
        self.read_datetime().map_err(map_goldfish_rtc_error)
    }

    fn set_time(&self, time: RtcDateTime) -> Result<(), RtcError> {
        self.write_datetime(time).map_err(map_goldfish_rtc_error)
    }

    fn read_alarm(&self) -> Result<RtcAlarm, RtcError> {
        self.read_alarm_datetime().map_err(map_goldfish_rtc_error)
    }

    fn set_alarm(&self, alarm: RtcAlarm) -> Result<(), RtcError> {
        self.write_alarm_datetime(alarm)
            .map_err(map_goldfish_rtc_error)
    }

    fn set_alarm_irq_enabled(&self, enabled: bool) -> Result<(), RtcError> {
        if !self.alarm_irq_available() {
            return Err(RtcError::Unsupported);
        }
        self.set_alarm_enabled(enabled)
            .map_err(map_goldfish_rtc_error)
    }

    fn features(&self) -> RtcFeatures {
        let mut features = RtcFeatures::READ_TIME.with(RtcFeatures::SET_TIME);
        if self.size >= RTC_ALARM_MIN_SIZE {
            features = features.with(RtcFeatures::ALARM);
        }
        if self.size >= RTC_ALARM_MIN_SIZE && self.alarm_irq_available() {
            features = features.with(RtcFeatures::ALARM_IRQ);
        }
        features
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn map_goldfish_rtc_error(err: GoldfishRtcError) -> RtcError {
    match err {
        GoldfishRtcError::RegisterWindowTooSmall => RtcError::Io,
        GoldfishRtcError::AlarmUnsupported => RtcError::Unsupported,
        GoldfishRtcError::InvalidDate => RtcError::Invalid,
    }
}

struct GoldfishRtcBinding {
    rtc: Arc<GoldfishRtc>,
    rtc_dev: Arc<RtcDevice>,
}

struct GoldfishRtcIrqHandler {
    rtc: Arc<GoldfishRtc>,
    rtc_dev: Weak<RtcDevice>,
}

impl IrqHandler for GoldfishRtcIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        match self.rtc.acknowledge_alarm_irq() {
            Ok(()) => {
                if let Some(rtc_dev) = self.rtc_dev.upgrade() {
                    let _ = rtc_dev.record_irq(RtcIrqData::new(1, RtcIrqFlags::ALARM));
                }
                IrqStatus::Handled
            }
            Err(_) => IrqStatus::Unhandled,
        }
    }
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

    fn register_alarm_irq_handler(
        &self,
        rtc: &Arc<GoldfishRtc>,
        rtc_dev: &Arc<RtcDevice>,
        info: &PlatformDeviceInfo,
    ) -> Result<Option<IrqHandle>, PnpError> {
        if rtc.size < RTC_ALARM_MIN_SIZE {
            return Ok(None);
        }
        let handler: Arc<dyn IrqHandler> = Arc::new(GoldfishRtcIrqHandler {
            rtc: Arc::clone(rtc),
            rtc_dev: Arc::downgrade(rtc_dev),
        });
        match info.register_first_irq_handler(handler) {
            Ok(handle) => {
                rtc.set_alarm_irq_available(true);
                Ok(Some(handle))
            }
            Err(PlatformIrqRegistrationError::NoResource) => Ok(None),
            Err(PlatformIrqRegistrationError::Unresolved) => {
                Err(PnpError::dependency(first_irq_dependency(info)))
            }
            Err(PlatformIrqRegistrationError::RegistrationFailed { err, .. }) => match err {
                irq::IrqError::OutOfMemory => Err(PnpError::OutOfMemory),
                irq::IrqError::AlreadyRegistered => Err(PnpError::registration_failed(
                    PnpResourceKind::Irq,
                    "goldfish rtc alarm irq already registered",
                )),
                irq::IrqError::NotFound => Err(PnpError::registration_failed(
                    PnpResourceKind::Irq,
                    "goldfish rtc alarm irq line not found",
                )),
            },
        }
    }
}

fn first_irq_dependency(info: &PlatformDeviceInfo) -> PnpDependency {
    info.irq_resources()
        .find_map(|irq| irq.controller())
        .map(PnpDependency::IrqController)
        .unwrap_or(PnpDependency::DefaultIrqDomain)
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

        let rtc_driver: Arc<dyn RtcDriver> = rtc.clone();
        let rtc_projection_name = RtcDevice::alloc_stable_projection_name(&dev.name)?;
        let rtc_dev = Arc::new(RtcDevice::new(rtc_projection_name, rtc_driver));
        dev.register_function(RtcFunction::new_arc(Arc::clone(&rtc_dev)))?;
        let irq_handle = self.register_alarm_irq_handler(&rtc, &rtc_dev, info)?;
        if let Some(handle) = irq_handle
            && let Err(err) = dev.own_resource(irq::irq_handler_pnp_resource(
                handle,
                "platform-goldfish-rtc-alarm",
            ))
        {
            rtc.set_alarm_irq_available(false);
            let _ = irq::unregister_irq_handler(handle);
            return Err(err);
        }
        self.install_realtime_clock(dev, phys, realtime_ns);
        dev.set_driver_data(Arc::new(GoldfishRtcBinding { rtc, rtc_dev }));
        log::printk!(
            "[platform-goldfish-rtc] alarm support={} irq={}",
            (size >= RTC_ALARM_MIN_SIZE) as usize,
            irq_handle.is_some() as usize
        );
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
            binding.rtc.set_alarm_irq_available(false);
            let _ = binding.rtc.clear_alarm();
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
