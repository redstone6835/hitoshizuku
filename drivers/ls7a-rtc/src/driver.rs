//! Loongson LS7A RTC platform ELM 驱动。
//!
//! 固件层只负责把 `compatible` 与 MMIO resource 注册成 platform 设备。本模块
//! 负责匹配 `loongson,ls7a-rtc`、访问 LS7A RTC 寄存器，并把读到的硬件时间交给
//! 内核 realtime 时钟回调。

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

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
use vfs::sync::Spinlock;

const COMPAT_LOONGSON_LS7A_RTC: &str = "loongson,ls7a-rtc";

const TOY_WRITE0_REG: usize = 0x24;
const TOY_WRITE1_REG: usize = 0x28;
const TOY_READ0_REG: usize = 0x2c;
const TOY_READ1_REG: usize = 0x30;
const TOY_MATCH0_REG: usize = 0x34;
const CTRL_REG: usize = 0x40;
const MIN_REG_SIZE: usize = CTRL_REG + core::mem::size_of::<u32>();
const MIN_ALARM_REG_SIZE: usize = TOY_MATCH0_REG + core::mem::size_of::<u32>();

const PM1_STS_REG: usize = 0x0c;
const PM1_EN_REG: usize = 0x10;
const PM_RTC_BIT: u32 = 1 << 10;
const CTRL_OSC_ENABLE: u32 = 1 << 8;
const CTRL_TOY_ENABLE: u32 = 1 << 11;
const CTRL_REQUIRED: u32 = CTRL_OSC_ENABLE | CTRL_TOY_ENABLE;
const NO_REALTIME_SOURCE: usize = 0;

/// LS7A TOY year 寄存器存储的是从 1900 开始的年偏移。
const TOY_YEAR_BASE: u32 = 1900;
/// TOY_MATCH0 只保存 6 bit 年份；驱动用当前 RTC 年份所在的 64 年窗口补齐高位。
const TOY_ALARM_YEAR_WINDOW: u32 = 64;
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

const MATCH_SECOND_SHIFT: u32 = 0;
const MATCH_SECOND_MASK: u32 = 0x3f;
const MATCH_MINUTE_SHIFT: u32 = 6;
const MATCH_MINUTE_MASK: u32 = 0x3f;
const MATCH_HOUR_SHIFT: u32 = 12;
const MATCH_HOUR_MASK: u32 = 0x1f;
const MATCH_DAY_SHIFT: u32 = 17;
const MATCH_DAY_MASK: u32 = 0x1f;
const MATCH_MONTH_SHIFT: u32 = 22;
const MATCH_MONTH_MASK: u32 = 0x0f;
const MATCH_YEAR_SHIFT: u32 = 26;
const MATCH_YEAR_MASK: u32 = 0x3f;

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
    pm_base: Option<usize>,
    alarm_irq_available: AtomicBool,
    alarm_enabled: AtomicBool,
    alarm_match: AtomicU32,
    fix_year_offset: AtomicU32,
    pm_lock: Spinlock<()>,
}

impl Ls7aRtc {
    pub const fn new(base: usize, size: usize, pm_base: Option<usize>) -> Self {
        Self {
            base,
            size,
            pm_base,
            alarm_irq_available: AtomicBool::new(false),
            alarm_enabled: AtomicBool::new(false),
            alarm_match: AtomicU32::new(0),
            fix_year_offset: AtomicU32::new(0),
            pm_lock: Spinlock::new(()),
        }
    }

    pub fn read_unix_time_ns(&self) -> Result<u64, Ls7aRtcError> {
        self.read_datetime()?
            .unix_time_ns()
            .ok_or(Ls7aRtcError::InvalidDate)
    }

    fn read_datetime(&self) -> Result<RtcDateTime, Ls7aRtcError> {
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
            self.update_fix_year(year0);
            return RtcDateTime::new(year, month, day, hour, minute, second)
                .ok_or(Ls7aRtcError::InvalidDate);
        }

        Err(Ls7aRtcError::UnstableRead)
    }

    fn write_datetime(&self, time: RtcDateTime) -> Result<(), Ls7aRtcError> {
        self.ensure_register_window()?;
        let year = time
            .year
            .checked_sub(TOY_YEAR_BASE)
            .ok_or(Ls7aRtcError::InvalidDate)?;
        let low = encode_toy_low(time);

        // LS7A TOY 写入口与读入口同样拆成两个寄存器：WRITE0 保存日历低字段，
        // WRITE1 保存从 1900 开始的年偏移。写完后再确认计数器启用，避免把硬件
        // 寄存器时序暴露到 RTC class 层。
        self.write32(TOY_WRITE0_REG, low)?;
        self.write32(TOY_WRITE1_REG, year)?;
        self.update_fix_year(year);
        self.enable_counter()
    }

    fn read_alarm_datetime(&self) -> Result<RtcAlarm, Ls7aRtcError> {
        self.ensure_alarm_register_window()?;
        let fix_year = self.ensure_fix_year()?;
        let hardware_raw = self.read32(TOY_MATCH0_REG)?;
        let raw = if hardware_raw != 0 {
            self.alarm_match.store(hardware_raw, Ordering::Release);
            hardware_raw
        } else {
            self.alarm_match.load(Ordering::Acquire)
        };

        let second = (raw >> MATCH_SECOND_SHIFT) & MATCH_SECOND_MASK;
        let minute = (raw >> MATCH_MINUTE_SHIFT) & MATCH_MINUTE_MASK;
        let hour = (raw >> MATCH_HOUR_SHIFT) & MATCH_HOUR_MASK;
        let day = (raw >> MATCH_DAY_SHIFT) & MATCH_DAY_MASK;
        let month = (raw >> MATCH_MONTH_SHIFT) & MATCH_MONTH_MASK;
        let year = ((raw >> MATCH_YEAR_SHIFT) & MATCH_YEAR_MASK)
            .checked_add(fix_year)
            .and_then(|offset| offset.checked_add(TOY_YEAR_BASE))
            .ok_or(Ls7aRtcError::Overflow)?;

        let time = RtcDateTime::new(year, month, day, hour, minute, second)
            .ok_or(Ls7aRtcError::InvalidDate)?;
        let (enabled, pending) = if let Some(pm_base) = self.pm_base {
            let _guard = self.pm_lock.lock();
            (
                self.pm_read32(pm_base, PM1_EN_REG)? & PM_RTC_BIT != 0,
                self.pm_read32(pm_base, PM1_STS_REG)? & PM_RTC_BIT != 0,
            )
        } else {
            (self.alarm_enabled.load(Ordering::Acquire), false)
        };
        Ok(RtcAlarm {
            time,
            enabled,
            pending,
        })
    }

    fn write_alarm_datetime(&self, alarm: RtcAlarm) -> Result<(), Ls7aRtcError> {
        self.ensure_alarm_register_window()?;
        let raw = self.encode_alarm_match(alarm.time)?;
        self.alarm_match.store(raw, Ordering::Release);
        if self.pm_base.is_some() {
            self.write32(TOY_MATCH0_REG, raw)?;
        }
        self.set_alarm_enabled(alarm.enabled)
    }

    fn set_alarm_enabled(&self, enabled: bool) -> Result<(), Ls7aRtcError> {
        self.ensure_alarm_register_window()?;
        if let Some(pm_base) = self.pm_base {
            let _guard = self.pm_lock.lock();
            // PM1_STS 是 write-one-to-clear。设置 enable 前先清旧 pending，避免
            // 用户刚写入的新 alarm 立即继承上一次事件状态。
            self.pm_write32(pm_base, PM1_STS_REG, PM_RTC_BIT)?;
            let mut value = self.pm_read32(pm_base, PM1_EN_REG)?;
            if enabled {
                value |= PM_RTC_BIT;
            } else {
                value &= !PM_RTC_BIT;
            }
            self.pm_write32(pm_base, PM1_EN_REG, value)?;
        } else {
            // DT 直连 IRQ 没有独立 enable 位。保留用户设置的 match 值，并用
            // TOY_MATCH0 是否为 0 表达启停，避免访问固件没有声明的 PM 窗口。
            let raw = if enabled {
                self.alarm_match.load(Ordering::Acquire)
            } else {
                0
            };
            self.write32(TOY_MATCH0_REG, raw)?;
        }
        self.alarm_enabled.store(enabled, Ordering::Release);
        Ok(())
    }

    fn acknowledge_alarm_irq(&self) -> Result<bool, Ls7aRtcError> {
        self.ensure_alarm_register_window()?;
        // DT 路径使用 RTC 控制器自己的 IRQ，而不是 ACPI fixed event。Linux 的
        // platform ISR 同样只清零 TOY_MATCH0；PM1_STS/PM1_EN 属于独立的 ACPI
        // fixed-event/wakeup 语义，不能在直连 IRQ handler 中无条件访问。
        self.write32(TOY_MATCH0_REG, 0)?;
        self.alarm_enabled.store(false, Ordering::Release);
        Ok(true)
    }

    fn set_alarm_irq_available(&self, available: bool) {
        self.alarm_irq_available.store(available, Ordering::Release);
    }

    fn alarm_irq_available(&self) -> bool {
        self.alarm_irq_available.load(Ordering::Acquire)
    }

    fn ensure_alarm_register_window(&self) -> Result<(), Ls7aRtcError> {
        if self.size != 0 && self.size < MIN_ALARM_REG_SIZE {
            return Err(Ls7aRtcError::RegisterWindowTooSmall);
        }
        Ok(())
    }

    fn alarm_supported(&self) -> bool {
        self.size == 0 || self.size >= MIN_ALARM_REG_SIZE
    }

    fn alarm_irq_supported(&self) -> bool {
        self.alarm_supported()
    }

    fn ensure_fix_year(&self) -> Result<u32, Ls7aRtcError> {
        let current = self.fix_year_offset.load(Ordering::Acquire);
        if current != 0 {
            return Ok(current);
        }
        // probe() 通常会先读当前时间并初始化 fix_year。这里保留懒加载兜底，
        // 让未来热插拔路径即使先读 alarm，也不依赖设备注册顺序。
        let _ = self.read_datetime()?;
        Ok(self.fix_year_offset.load(Ordering::Acquire))
    }

    fn update_fix_year(&self, year_offset: u32) {
        let fix_year = (year_offset / TOY_ALARM_YEAR_WINDOW) * TOY_ALARM_YEAR_WINDOW;
        self.fix_year_offset.store(fix_year, Ordering::Release);
    }

    fn encode_alarm_match(&self, time: RtcDateTime) -> Result<u32, Ls7aRtcError> {
        let year_offset = time
            .year
            .checked_sub(TOY_YEAR_BASE)
            .ok_or(Ls7aRtcError::InvalidDate)?;
        let fix_year = self.ensure_fix_year()?;
        let match_year = year_offset
            .checked_sub(fix_year)
            .filter(|value| *value < TOY_ALARM_YEAR_WINDOW)
            .ok_or(Ls7aRtcError::InvalidDate)?;

        Ok((time.second & MATCH_SECOND_MASK) << MATCH_SECOND_SHIFT
            | (time.minute & MATCH_MINUTE_MASK) << MATCH_MINUTE_SHIFT
            | (time.hour & MATCH_HOUR_MASK) << MATCH_HOUR_SHIFT
            | (time.day & MATCH_DAY_MASK) << MATCH_DAY_SHIFT
            | (time.month & MATCH_MONTH_MASK) << MATCH_MONTH_SHIFT
            | (match_year & MATCH_YEAR_MASK) << MATCH_YEAR_SHIFT)
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
        // Safety: `ensure_register_window` 在访问前验证主 RTC 窗口，所有偏移均为对齐的
        // 固定寄存器偏移，且基址由 platform probe 完成映射。
        Ok(unsafe { core::ptr::read_volatile(addr as *const u32) })
    }

    fn write32(&self, offset: usize, value: u32) -> Result<(), Ls7aRtcError> {
        let addr = self
            .base
            .checked_add(offset)
            .ok_or(Ls7aRtcError::Overflow)?;
        // Safety: 安全条件与 `read32` 相同，目标 RTC 寄存器允许 32 位易失写入。
        unsafe { core::ptr::write_volatile(addr as *mut u32, value) };
        Ok(())
    }

    fn pm_read32(&self, pm_base: usize, offset: usize) -> Result<u32, Ls7aRtcError> {
        let addr = pm_base.checked_add(offset).ok_or(Ls7aRtcError::Overflow)?;
        // Safety: `pm_base` 来自已映射并验证尺寸的 ACPI PM 寄存器窗口，调用方只传入
        // 该窗口内的对齐固定偏移。
        Ok(unsafe { core::ptr::read_volatile(addr as *const u32) })
    }

    fn pm_write32(&self, pm_base: usize, offset: usize, value: u32) -> Result<(), Ls7aRtcError> {
        let addr = pm_base.checked_add(offset).ok_or(Ls7aRtcError::Overflow)?;
        // Safety: 安全条件与 `pm_read32` 相同，目标 PM 寄存器允许 32 位易失写入。
        unsafe { core::ptr::write_volatile(addr as *mut u32, value) };
        Ok(())
    }
}

impl RtcDriver for Ls7aRtc {
    fn read_time(&self) -> Result<RtcDateTime, RtcError> {
        self.read_datetime().map_err(map_ls7a_rtc_error)
    }

    fn set_time(&self, time: RtcDateTime) -> Result<(), RtcError> {
        self.write_datetime(time).map_err(map_ls7a_rtc_error)
    }

    fn read_alarm(&self) -> Result<RtcAlarm, RtcError> {
        self.read_alarm_datetime().map_err(map_ls7a_rtc_error)
    }

    fn set_alarm(&self, alarm: RtcAlarm) -> Result<(), RtcError> {
        self.write_alarm_datetime(alarm).map_err(map_ls7a_rtc_error)
    }

    fn set_alarm_irq_enabled(&self, enabled: bool) -> Result<(), RtcError> {
        if !self.alarm_irq_available() {
            return Err(RtcError::Unsupported);
        }
        self.set_alarm_enabled(enabled).map_err(map_ls7a_rtc_error)
    }

    fn features(&self) -> RtcFeatures {
        let mut features = RtcFeatures::READ_TIME.with(RtcFeatures::SET_TIME);
        if self.alarm_supported() {
            features = features.with(RtcFeatures::ALARM);
        }
        if self.alarm_irq_supported() && self.alarm_irq_available() {
            features = features.with(RtcFeatures::ALARM_IRQ);
        }
        // class 层只消费已声明且可确认的事件源。当前驱动只把已接入 IRQ
        // domain 的 alarm 事件声明为能力，避免把未接线的 update/periodic
        // 事件暴露成可用接口。
        features
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn map_ls7a_rtc_error(err: Ls7aRtcError) -> RtcError {
    match err {
        Ls7aRtcError::RegisterWindowTooSmall
        | Ls7aRtcError::InvalidDate
        | Ls7aRtcError::Overflow => RtcError::Invalid,
        Ls7aRtcError::CounterDisabled => RtcError::Io,
        Ls7aRtcError::UnstableRead => RtcError::Busy,
    }
}

fn encode_toy_low(time: RtcDateTime) -> u32 {
    (time.second & TOY_SECOND_MASK) << TOY_SECOND_SHIFT
        | (time.minute & TOY_MINUTE_MASK) << TOY_MINUTE_SHIFT
        | (time.hour & TOY_HOUR_MASK) << TOY_HOUR_SHIFT
        | (time.day & TOY_DAY_MASK) << TOY_DAY_SHIFT
        | (time.month & TOY_MONTH_MASK) << TOY_MONTH_SHIFT
}

struct Ls7aRtcIrqHandler {
    rtc: Arc<Ls7aRtc>,
    rtc_dev: Weak<RtcDevice>,
}

impl IrqHandler for Ls7aRtcIrqHandler {
    fn handle_irq(&self, _line: IrqLine) -> IrqStatus {
        match self.rtc.acknowledge_alarm_irq() {
            Ok(true) => {
                if let Some(rtc_dev) = self.rtc_dev.upgrade() {
                    let _ = rtc_dev.record_irq(RtcIrqData::new(1, RtcIrqFlags::ALARM));
                }
                IrqStatus::Handled
            }
            Ok(false) | Err(_) => IrqStatus::Unhandled,
        }
    }
}

struct Ls7aRtcBinding {
    rtc: Arc<Ls7aRtc>,
    rtc_dev: Arc<RtcDevice>,
}

pub struct Ls7aRtcPlatformDriver {
    device_mmio_to_virt: fn(usize) -> usize,
    set_realtime_ns: Option<fn(u64)>,
    install_realtime_source: Option<fn(RealtimeClockSource) -> bool>,
    unregister_realtime_source: Option<fn(usize)>,
    realtime_owner: AtomicUsize,
}

fn first_irq_dependency(info: &PlatformDeviceInfo) -> PnpDependency {
    info.irq_resources()
        .find_map(|irq| irq.controller())
        .map(PnpDependency::IrqController)
        .unwrap_or(PnpDependency::DefaultIrqDomain)
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

    fn register_alarm_irq_handler(
        &self,
        rtc: &Arc<Ls7aRtc>,
        rtc_dev: &Arc<RtcDevice>,
        info: &PlatformDeviceInfo,
    ) -> Result<Option<IrqHandle>, PnpError> {
        if !rtc.alarm_irq_supported() {
            return Ok(None);
        }
        let handler: Arc<dyn IrqHandler> = Arc::new(Ls7aRtcIrqHandler {
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
                log::debug!(
                    "[platform-ls7a-rtc] {} has firmware IRQ resource but no registered IRQ domain translator",
                    info.fw_name.as_ref()
                );
                Err(PnpError::dependency(first_irq_dependency(info)))
            }
            Err(PlatformIrqRegistrationError::RegistrationFailed { line, err }) => {
                log::printk!(
                    "[platform-ls7a-rtc] failed to register alarm irq {:?}: {:?}",
                    line,
                    err
                );
                match err {
                    irq::IrqError::OutOfMemory => Err(PnpError::OutOfMemory),
                    irq::IrqError::AlreadyRegistered => Err(PnpError::registration_failed(
                        PnpResourceKind::Irq,
                        "rtc alarm irq already registered",
                    )),
                    irq::IrqError::NotFound => Err(PnpError::registration_failed(
                        PnpResourceKind::Irq,
                        "rtc alarm irq line not found",
                    )),
                }
            }
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
            return Err(PnpError::missing(PnpResourceKind::Mmio, "rtc reg missing"));
        };
        let pm_base =
            firmware_pm_mmio(info).map(|(pm_phys, _)| (self.device_mmio_to_virt)(pm_phys));
        let rtc = Arc::new(Ls7aRtc::new(
            (self.device_mmio_to_virt)(phys),
            size,
            pm_base,
        ));
        let realtime_ns = rtc.read_unix_time_ns().map_err(|err| {
            log::printk!(
                "[platform-ls7a-rtc] probe failed for {} phys={:#x}: {:?}",
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
                "platform-ls7a-rtc-alarm",
            ))
        {
            rtc.set_alarm_irq_available(false);
            let _ = irq::unregister_irq_handler(handle);
            return Err(err);
        }

        self.install_realtime_clock(dev, phys, realtime_ns);

        dev.set_driver_data(Arc::new(Ls7aRtcBinding { rtc, rtc_dev }));
        Ok(())
    }

    fn remove(&self, dev: &Arc<PnpDevice>) {
        if let Some(info) = dev.info.as_any().downcast_ref::<PlatformDeviceInfo>()
            && let Some((phys, _)) = info.first_mmio()
        {
            self.unregister_realtime_clock(dev, phys);
        }
        if let Some(data) = dev.take_driver_data()
            && let Ok(binding) = data.downcast::<Ls7aRtcBinding>()
        {
            binding.rtc.set_alarm_irq_available(false);
            let _ = binding.rtc.set_alarm_enabled(false);
            binding.rtc_dev.mark_gone();
        }
        log::printk!("[platform-ls7a-rtc] removed {}", dev.id);
    }
}

fn firmware_pm_mmio(info: &PlatformDeviceInfo) -> Option<(usize, usize)> {
    if info.string_list_property("reg-names").is_some() {
        return info.mmio_by_name(&["pm", "acpi", "alarm", "rtc-pm"]);
    }
    second_mmio(info)
}

fn second_mmio(info: &PlatformDeviceInfo) -> Option<(usize, usize)> {
    // 固件如果声明了额外 MMIO resource，第一段仍是 RTC TOY 窗口，第二段才作为
    // alarm enable/status 所需的 PM 窗口。固件只声明 RTC 子窗口时不能从地址
    // 反推 PM 基址；QEMU LoongArch 的相邻区域并未映射，越界访问会产生总线异常。
    info.mmio_at(1)
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

pub(super) fn register_builtin_driver() -> Result<DriverHandle, PnpError> {
    register_driver_factory(Arc::new(Ls7aRtcFactory))
}
