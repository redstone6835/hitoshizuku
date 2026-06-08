//! 通用 RTC 设备抽象与 `/dev/rtc*` 兼容接口。
//!
//! 本模块分三层：
//!
//! 1. [`RtcDriver`] 描述硬件驱动能力，只表达平台无关的 RTC 语义；
//! 2. [`RtcDevice`] 为驱动实例提供生命周期、typed control 和统一错误映射；
//! 3. [`RtcFunction`] 把 RTC 设备投影成 devtmpfs 自定义节点，由这里的
//!    [`InodeOps`] / [`FileOps`] 负责 RTC ioctl ABI 解码。
//!
//! 这样 LS7A、CMOS 或其它未来 RTC 驱动只需要实现 [`RtcDriver`]，不需要在
//! devtmpfs 或 syscall 层增加设备类型特判。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicU8, Ordering};

use errno::Errno;
use sched::{Task, WaitQueue};
use vfs::cred::{Capability, Credentials};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeOps};
use vfs::stat::{FileMode, FileType};
use vfs::sync::Spinlock;

use crate::dev::control::DriverControl;
use crate::dev::function::{
    CustomDevNodeSpec, DevNodeName, DevNodeNameAllocError, DevNodeNameAllocator, DevNodeSet,
    DevNodeSpec, DeviceClassId, DeviceFunction,
};
use crate::mm::{copy_from_user, copy_to_user};

const NSEC_PER_SEC: u64 = 1_000_000_000;
const SECS_PER_DAY: u64 = 86_400;
const UNIX_EPOCH_WEEKDAY: u32 = 4; // 1970-01-01 为周四，tm_wday 约定周日为 0。
const RTC_TIME_LEN: usize = 9 * core::mem::size_of::<i32>();
const RTC_WKALRM_LEN: usize = 4 + RTC_TIME_LEN;
static RTC_DEV_NAMES: DevNodeNameAllocator = DevNodeNameAllocator::new("rtc");
const RTC_READ_WORD_LEN: usize = core::mem::size_of::<usize>();
const RTC_DEFAULT_EPOCH: u32 = 1900;
const RTC_DEFAULT_PERIODIC_RATE_HZ: u32 = 1024;
const RTC_MIN_PERIODIC_RATE_HZ: u32 = 2;
const RTC_MAX_PERIODIC_RATE_HZ: u32 = 8192;

const RTC_IRQF: u32 = 0x80;
const RTC_PF: u32 = 0x40;
const RTC_AF: u32 = 0x20;
const RTC_UF: u32 = 0x10;

const RTC_AIE_ON: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, b'p' as usize, 0x01, 0).raw();
const RTC_AIE_OFF: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, b'p' as usize, 0x02, 0).raw();
const RTC_UIE_ON: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, b'p' as usize, 0x03, 0).raw();
const RTC_UIE_OFF: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, b'p' as usize, 0x04, 0).raw();
const RTC_PIE_ON: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, b'p' as usize, 0x05, 0).raw();
const RTC_PIE_OFF: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, b'p' as usize, 0x06, 0).raw();
const RTC_ALM_SET: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'p' as usize, 0x07, RTC_TIME_LEN).raw();
const RTC_ALM_READ: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, b'p' as usize, 0x08, RTC_TIME_LEN).raw();
const RTC_RD_TIME: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, b'p' as usize, 0x09, RTC_TIME_LEN).raw();
const RTC_SET_TIME: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'p' as usize, 0x0a, RTC_TIME_LEN).raw();
const RTC_IRQP_READ: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, b'p' as usize, 0x0b, RTC_READ_WORD_LEN).raw();
const RTC_IRQP_SET: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'p' as usize, 0x0c, RTC_READ_WORD_LEN).raw();
const RTC_EPOCH_READ: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, b'p' as usize, 0x0d, RTC_READ_WORD_LEN).raw();
const RTC_EPOCH_SET: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'p' as usize, 0x0e, RTC_READ_WORD_LEN).raw();
const RTC_WKALM_SET: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'p' as usize, 0x0f, RTC_WKALRM_LEN).raw();
const RTC_WKALM_RD: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, b'p' as usize, 0x10, RTC_WKALRM_LEN).raw();
const RTC_VL_READ: usize = IoctlCmd::from_parts(
    IoctlCmd::IOC_READ,
    b'p' as usize,
    0x13,
    core::mem::size_of::<u32>(),
)
.raw();
const RTC_VL_CLR: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, b'p' as usize, 0x14, 0).raw();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcDateTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl RtcDateTime {
    pub fn new(
        year: u32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<Self> {
        let value = Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        };
        value.is_valid().then_some(value)
    }

    pub fn from_unix_time_ns(ns: u64) -> Option<Self> {
        let mut days = ns / NSEC_PER_SEC / SECS_PER_DAY;
        let seconds_of_day = (ns / NSEC_PER_SEC) % SECS_PER_DAY;

        let mut year = 1970u32;
        loop {
            let year_days = if is_leap_year(year) { 366 } else { 365 };
            if days < year_days {
                break;
            }
            days -= year_days;
            year = year.checked_add(1)?;
            if year > 9999 {
                return None;
            }
        }

        let mut month = 1u32;
        loop {
            let month_days = days_in_month(year, month)? as u64;
            if days < month_days {
                break;
            }
            days -= month_days;
            month += 1;
        }

        Self::new(
            year,
            month,
            days as u32 + 1,
            (seconds_of_day / 3_600) as u32,
            ((seconds_of_day % 3_600) / 60) as u32,
            (seconds_of_day % 60) as u32,
        )
    }

    pub fn unix_time_ns(self) -> Option<u64> {
        let seconds = self.days_since_unix_epoch()?.checked_mul(SECS_PER_DAY)?;
        seconds
            .checked_add((self.hour as u64).checked_mul(3_600)?)?
            .checked_add((self.minute as u64).checked_mul(60)?)?
            .checked_add(self.second as u64)?
            .checked_mul(NSEC_PER_SEC)
    }

    pub fn weekday(self) -> Option<u32> {
        Some(((self.days_since_unix_epoch()? as u32) + UNIX_EPOCH_WEEKDAY) % 7)
    }

    pub fn yearday0(self) -> Option<u32> {
        if !self.is_valid() {
            return None;
        }
        let mut days = 0u32;
        for month in 1..self.month {
            days = days.checked_add(days_in_month(self.year, month)?)?;
        }
        days.checked_add(self.day - 1)
    }

    fn days_since_unix_epoch(self) -> Option<u64> {
        if !self.is_valid() {
            return None;
        }

        let mut days = 0u64;
        for year in 1970..self.year {
            days = days.checked_add(if is_leap_year(year) { 366 } else { 365 })?;
        }
        for month in 1..self.month {
            days = days.checked_add(days_in_month(self.year, month)? as u64)?;
        }
        days.checked_add((self.day - 1) as u64)
    }

    fn is_valid(self) -> bool {
        if !(1970..=9999).contains(&self.year) {
            return false;
        }
        if self.hour > 23 || self.minute > 59 || self.second > 59 {
            return false;
        }
        let Some(month_days) = days_in_month(self.year, self.month) else {
            return false;
        };
        self.day != 0 && self.day <= month_days
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcAlarm {
    pub time: RtcDateTime,
    pub enabled: bool,
    pub pending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcIrqData {
    pub count: u32,
    pub flags: RtcIrqFlags,
}

impl RtcIrqData {
    pub const fn new(count: u32, flags: RtcIrqFlags) -> Self {
        Self { count, flags }
    }

    fn linux_word(self) -> usize {
        ((self.count as usize) << 8) | self.flags.linux_bits() as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcIrqFlags(u32);

impl RtcIrqFlags {
    pub const PERIODIC: Self = Self(RTC_IRQF | RTC_PF);
    pub const ALARM: Self = Self(RTC_IRQF | RTC_AF);
    pub const UPDATE: Self = Self(RTC_IRQF | RTC_UF);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn linux_bits(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcFeatures(u32);

impl RtcFeatures {
    pub const READ_TIME: Self = Self(1 << 0);
    pub const SET_TIME: Self = Self(1 << 1);
    pub const ALARM: Self = Self(1 << 2);
    pub const ALARM_IRQ: Self = Self(1 << 3);
    pub const UPDATE_IRQ: Self = Self(1 << 4);
    pub const PERIODIC_IRQ: Self = Self(1 << 5);
    pub const VOLTAGE_LOW: Self = Self(1 << 6);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcError {
    Unsupported,
    Invalid,
    NoDevice,
    Busy,
    WouldBlock,
    Io,
    Permission,
}

impl RtcError {
    fn to_errno(self) -> Errno {
        match self {
            Self::Unsupported => Errno::ENOTTY,
            Self::Invalid => Errno::EINVAL,
            Self::NoDevice => Errno::ENODEV,
            Self::Busy => Errno::EBUSY,
            Self::WouldBlock => Errno::EAGAIN,
            Self::Io => Errno::EIO,
            Self::Permission => Errno::EPERM,
        }
    }

    fn to_vfs(self) -> VfsError {
        match self {
            Self::Unsupported => VfsError::NotSupported,
            Self::Invalid => VfsError::InvalidArgument,
            Self::NoDevice => VfsError::NoDevice,
            Self::Busy => VfsError::DeviceBusy,
            Self::WouldBlock => VfsError::WouldBlock,
            Self::Io => VfsError::Io,
            Self::Permission => VfsError::OperationNotPermitted,
        }
    }
}

/// 硬件 RTC 驱动的 typed 语义接口。
///
/// 该 trait 只表达 RTC 类设备的硬件能力，不能解析 ioctl number 或用户指针。
/// `features()` 是契约的一部分：声明某个 feature 后，对应方法必须提供真实实现；
/// 无法由硬件或当前中断框架完成的能力应保持未声明并返回 [`RtcError::Unsupported`]。
pub trait RtcDriver: Send + Sync {
    fn read_time(&self) -> Result<RtcDateTime, RtcError>;

    fn set_time(&self, _time: RtcDateTime) -> Result<(), RtcError> {
        Err(RtcError::Unsupported)
    }

    fn read_alarm(&self) -> Result<RtcAlarm, RtcError> {
        Err(RtcError::Unsupported)
    }

    fn set_alarm(&self, _alarm: RtcAlarm) -> Result<(), RtcError> {
        Err(RtcError::Unsupported)
    }

    fn set_alarm_irq_enabled(&self, _enabled: bool) -> Result<(), RtcError> {
        Err(RtcError::Unsupported)
    }

    fn set_update_irq_enabled(&self, _enabled: bool) -> Result<(), RtcError> {
        Err(RtcError::Unsupported)
    }

    fn set_periodic_irq_enabled(&self, _enabled: bool) -> Result<(), RtcError> {
        Err(RtcError::Unsupported)
    }

    /// 读取硬件当前 periodic IRQ 频率。
    ///
    /// 很多 RTC 只需要按 class 层缓存配置频率，不能从硬件无副作用读回；这类驱动
    /// 可以保持默认 [`RtcError::Unsupported`]，[`RtcDevice`] 会返回最近一次成功
    /// 配置的频率。
    fn read_periodic_rate(&self) -> Result<u32, RtcError> {
        Err(RtcError::Unsupported)
    }

    fn set_periodic_rate(&self, _hz: u32) -> Result<(), RtcError> {
        Err(RtcError::Unsupported)
    }

    fn read_voltage_low(&self) -> Result<u32, RtcError> {
        Err(RtcError::Unsupported)
    }

    fn clear_voltage_low(&self) -> Result<(), RtcError> {
        Err(RtcError::Unsupported)
    }

    fn features(&self) -> RtcFeatures {
        RtcFeatures::READ_TIME
    }

    fn as_any(&self) -> &dyn Any;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RtcDeviceState {
    Active = 0,
    Gone = 1,
}

pub struct RtcDevice {
    index: usize,
    name: Box<str>,
    driver: Arc<dyn RtcDriver>,
    state: AtomicU8,
    runtime: Spinlock<RtcRuntimeState>,
    irq_wait: WaitQueue,
}

/// RTC class 层维护的用户态兼容状态。
///
/// epoch、periodic rate、IRQ enable bit 和 pending IRQ word 都是 `/dev/rtc*`
/// ABI 的运行期状态，不属于某一种硬件寄存器布局。把它们放在 [`RtcDevice`]
/// 中可以让不同 RTC 驱动共享同一套 VFS/ioctl 行为，避免在 devtmpfs 或 syscall
/// 层按具体设备类型特判。
#[derive(Clone, Copy, Debug)]
struct RtcRuntimeState {
    epoch: u32,
    periodic_rate_hz: u32,
    alarm: Option<RtcAlarm>,
    alarm_irq_enabled: bool,
    update_irq_enabled: bool,
    periodic_irq_enabled: bool,
    pending_irq: Option<RtcIrqData>,
}

impl RtcRuntimeState {
    const fn new() -> Self {
        Self {
            epoch: RTC_DEFAULT_EPOCH,
            periodic_rate_hz: RTC_DEFAULT_PERIODIC_RATE_HZ,
            alarm: None,
            alarm_irq_enabled: false,
            update_irq_enabled: false,
            periodic_irq_enabled: false,
            pending_irq: None,
        }
    }

    fn irq_ready(&self) -> bool {
        self.pending_irq.is_some()
    }

    fn take_irq(&mut self) -> Option<RtcIrqData> {
        self.pending_irq.take()
    }

    fn accepts_irq(&self, flags: RtcIrqFlags) -> bool {
        (flags.contains(RtcIrqFlags::ALARM) && self.alarm_irq_enabled)
            || (flags.contains(RtcIrqFlags::UPDATE) && self.update_irq_enabled)
            || (flags.contains(RtcIrqFlags::PERIODIC) && self.periodic_irq_enabled)
    }

    fn push_irq(&mut self, data: RtcIrqData) {
        if data.flags.is_empty() {
            return;
        }
        let count = data.count.max(1);
        match self.pending_irq {
            Some(mut pending) => {
                pending.count = pending.count.saturating_add(count);
                pending.flags = pending.flags.with(data.flags);
                self.pending_irq = Some(pending);
            }
            None => {
                self.pending_irq = Some(RtcIrqData::new(count, data.flags));
            }
        }
    }
}

impl RtcDevice {
    pub fn new(node_name: DevNodeName, driver: Arc<dyn RtcDriver>) -> Self {
        let index = node_name.index();
        Self {
            index,
            name: node_name.into_string().into_boxed_str(),
            driver,
            state: AtomicU8::new(RtcDeviceState::Active as u8),
            runtime: Spinlock::new(RtcRuntimeState::new()),
            irq_wait: WaitQueue::new(),
        }
    }

    /// 为一个稳定硬件实例分配或复用 `/dev/rtc*` 节点名。
    ///
    /// `stable_key` 由 PnP 设备身份或固件路径提供。RTC core 统一管理兼容层命名，
    /// 具体硬件驱动只需要传入自身实例身份，避免在驱动里散落 `rtc{n}` 拼接逻辑。
    pub fn alloc_stable_node_name(stable_key: &str) -> Result<DevNodeName, DevNodeNameAllocError> {
        RTC_DEV_NAMES.try_alloc_stable(stable_key)
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn state(&self) -> RtcDeviceState {
        match self.state.load(Ordering::Acquire) {
            0 => RtcDeviceState::Active,
            _ => RtcDeviceState::Gone,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state() == RtcDeviceState::Active
    }

    pub fn mark_gone(&self) {
        self.state
            .store(RtcDeviceState::Gone as u8, Ordering::Release);
        self.irq_wait.wake_all();
    }

    pub fn read_time(&self) -> Result<RtcDateTime, RtcError> {
        self.ensure_active()?;
        self.driver.read_time()
    }

    pub fn set_time(&self, time: RtcDateTime) -> Result<(), RtcError> {
        self.ensure_active()?;
        self.require_feature(RtcFeatures::SET_TIME)?;
        self.driver.set_time(time)
    }

    /// 记录一个硬件 RTC IRQ 事件并唤醒 `/dev/rtc*` 等待者。
    ///
    /// 平台中断处理器在确认 alarm/update/periodic 事件后调用本方法。通用层会按
    /// 当前 enable bit 过滤事件、合并 pending word，并通过 WaitQueue 唤醒
    /// 阻塞 read/poll。这样 IRQ 分发路径不需要知道 ioctl ABI 的位布局。
    pub fn record_irq(&self, data: RtcIrqData) -> Result<(), RtcError> {
        self.ensure_active()?;
        let mut runtime = self.runtime.lock();
        if !runtime.accepts_irq(data.flags) {
            return Ok(());
        }
        runtime.push_irq(data);
        drop(runtime);
        self.irq_wait.wake_all();
        Ok(())
    }

    pub fn control(&self, req: RtcControlRequest) -> Result<RtcControlResponse, RtcError> {
        self.ensure_active()?;
        match req {
            RtcControlRequest::ReadTime => self.driver.read_time().map(RtcControlResponse::Time),
            RtcControlRequest::SetTime(time) => {
                self.set_time(time)?;
                Ok(RtcControlResponse::Done)
            }
            RtcControlRequest::ReadAlarm => self.read_alarm().map(RtcControlResponse::Alarm),
            RtcControlRequest::SetAlarm(alarm) => {
                self.set_alarm(alarm)?;
                Ok(RtcControlResponse::Done)
            }
            RtcControlRequest::SetAlarmIrqEnabled(enabled) => {
                self.set_alarm_irq_enabled(enabled)?;
                Ok(RtcControlResponse::Done)
            }
            RtcControlRequest::SetUpdateIrqEnabled(enabled) => {
                self.set_update_irq_enabled(enabled)?;
                Ok(RtcControlResponse::Done)
            }
            RtcControlRequest::SetPeriodicIrqEnabled(enabled) => {
                self.set_periodic_irq_enabled(enabled)?;
                Ok(RtcControlResponse::Done)
            }
            RtcControlRequest::ReadPeriodicRate => {
                self.read_periodic_rate().map(RtcControlResponse::U32)
            }
            RtcControlRequest::SetPeriodicRate(hz) => {
                self.set_periodic_rate(hz)?;
                Ok(RtcControlResponse::Done)
            }
            RtcControlRequest::ReadEpoch => Ok(RtcControlResponse::U32(self.read_epoch())),
            RtcControlRequest::SetEpoch(epoch) => {
                self.set_epoch(epoch)?;
                Ok(RtcControlResponse::Done)
            }
            RtcControlRequest::ReadVoltageLow => {
                self.require_feature(RtcFeatures::VOLTAGE_LOW)?;
                self.driver.read_voltage_low().map(RtcControlResponse::U32)
            }
            RtcControlRequest::ClearVoltageLow => {
                self.require_feature(RtcFeatures::VOLTAGE_LOW)?;
                self.driver.clear_voltage_low()?;
                Ok(RtcControlResponse::Done)
            }
            RtcControlRequest::ReadIrqData => self.read_irq_data().map(RtcControlResponse::IrqData),
            RtcControlRequest::GetFeatures => {
                Ok(RtcControlResponse::Features(self.driver.features()))
            }
        }
    }

    fn features(&self) -> RtcFeatures {
        self.driver.features()
    }

    fn require_feature(&self, feature: RtcFeatures) -> Result<(), RtcError> {
        if self.features().contains(feature) {
            Ok(())
        } else {
            Err(RtcError::Unsupported)
        }
    }

    fn read_alarm(&self) -> Result<RtcAlarm, RtcError> {
        self.require_feature(RtcFeatures::ALARM)?;
        let alarm = self.driver.read_alarm()?;
        self.runtime.lock().alarm = Some(alarm);
        Ok(alarm)
    }

    fn set_alarm(&self, alarm: RtcAlarm) -> Result<(), RtcError> {
        self.require_feature(RtcFeatures::ALARM)?;
        if alarm.enabled {
            // `rtc_wkalrm.enabled` 表示硬件 alarm 事件会被真正打开。没有
            // ALARM_IRQ 能力时只能安全地编程 match 时间，不能让设备产生一个
            // 当前内核无法确认和分发的中断源。
            self.require_feature(RtcFeatures::ALARM_IRQ)?;
        }
        self.driver.set_alarm(alarm)?;
        let mut runtime = self.runtime.lock();
        runtime.alarm = Some(alarm);
        runtime.alarm_irq_enabled = alarm.enabled;
        Ok(())
    }

    fn set_alarm_irq_enabled(&self, enabled: bool) -> Result<(), RtcError> {
        self.require_feature(RtcFeatures::ALARM_IRQ)?;
        self.driver.set_alarm_irq_enabled(enabled)?;
        let mut runtime = self.runtime.lock();
        runtime.alarm_irq_enabled = enabled;
        if let Some(mut alarm) = runtime.alarm {
            alarm.enabled = enabled;
            runtime.alarm = Some(alarm);
        }
        Ok(())
    }

    fn set_update_irq_enabled(&self, enabled: bool) -> Result<(), RtcError> {
        self.require_feature(RtcFeatures::UPDATE_IRQ)?;
        self.driver.set_update_irq_enabled(enabled)?;
        self.runtime.lock().update_irq_enabled = enabled;
        Ok(())
    }

    fn set_periodic_irq_enabled(&self, enabled: bool) -> Result<(), RtcError> {
        self.require_feature(RtcFeatures::PERIODIC_IRQ)?;
        self.driver.set_periodic_irq_enabled(enabled)?;
        self.runtime.lock().periodic_irq_enabled = enabled;
        Ok(())
    }

    fn read_periodic_rate(&self) -> Result<u32, RtcError> {
        self.require_feature(RtcFeatures::PERIODIC_IRQ)?;
        match self.driver.read_periodic_rate() {
            Ok(hz) => {
                validate_periodic_rate(hz)?;
                self.runtime.lock().periodic_rate_hz = hz;
                Ok(hz)
            }
            Err(RtcError::Unsupported) => Ok(self.runtime.lock().periodic_rate_hz),
            Err(err) => Err(err),
        }
    }

    fn set_periodic_rate(&self, hz: u32) -> Result<(), RtcError> {
        self.require_feature(RtcFeatures::PERIODIC_IRQ)?;
        validate_periodic_rate(hz)?;
        self.driver.set_periodic_rate(hz)?;
        self.runtime.lock().periodic_rate_hz = hz;
        Ok(())
    }

    fn read_epoch(&self) -> u32 {
        self.runtime.lock().epoch
    }

    fn set_epoch(&self, epoch: u32) -> Result<(), RtcError> {
        if epoch < 1900 {
            return Err(RtcError::Invalid);
        }
        self.runtime.lock().epoch = epoch;
        Ok(())
    }

    fn read_irq_data(&self) -> Result<RtcIrqData, RtcError> {
        self.ensure_active()?;
        self.runtime.lock().take_irq().ok_or(RtcError::WouldBlock)
    }

    fn irq_ready(&self) -> bool {
        self.runtime.lock().irq_ready()
    }

    fn add_irq_waiter(&self, task: &Arc<Task>) {
        self.irq_wait.enqueue(task);
    }

    fn remove_irq_waiter(&self, task: &Arc<Task>) {
        self.irq_wait.remove(task);
    }

    fn ensure_active(&self) -> Result<(), RtcError> {
        if self.is_active() {
            Ok(())
        } else {
            Err(RtcError::NoDevice)
        }
    }
}

impl DriverControl for Arc<RtcDevice> {
    type Request = RtcControlRequest;
    type Response = RtcControlResponse;
    type Error = RtcError;

    fn control(&self, req: Self::Request) -> Result<Self::Response, Self::Error> {
        RtcDevice::control(self, req)
    }
}

pub enum RtcControlRequest {
    ReadTime,
    SetTime(RtcDateTime),
    ReadAlarm,
    SetAlarm(RtcAlarm),
    SetAlarmIrqEnabled(bool),
    SetUpdateIrqEnabled(bool),
    SetPeriodicIrqEnabled(bool),
    ReadPeriodicRate,
    SetPeriodicRate(u32),
    ReadEpoch,
    SetEpoch(u32),
    ReadVoltageLow,
    ClearVoltageLow,
    ReadIrqData,
    GetFeatures,
}

pub enum RtcControlResponse {
    Done,
    Time(RtcDateTime),
    Alarm(RtcAlarm),
    IrqData(RtcIrqData),
    Features(RtcFeatures),
    U32(u32),
}

pub struct RtcFunction {
    dev: Arc<RtcDevice>,
}

impl RtcFunction {
    pub fn new(dev: Arc<RtcDevice>) -> Self {
        Self { dev }
    }

    pub fn dev(&self) -> Arc<RtcDevice> {
        Arc::clone(&self.dev)
    }
}

impl DeviceFunction for RtcFunction {
    fn class_id(&self) -> DeviceClassId {
        DeviceClassId::RTC
    }

    fn dev_name(&self) -> &str {
        self.dev.name()
    }

    fn mark_gone(&self) {
        self.dev.mark_gone();
    }

    fn devnodes(&self) -> Option<DevNodeSet> {
        let mut nodes = Vec::new();
        let ops = Arc::new(RtcInodeOps {
            dev: Arc::clone(&self.dev),
        });
        nodes.push(DevNodeSpec::custom(
            CustomDevNodeSpec::new(self.dev.name(), FileType::CharDevice, ops)
                .with_mode(FileMode::new(0o660)),
        ));
        if self.dev.index() == 0 {
            nodes.push(DevNodeSpec::Symlink {
                name: "rtc".into(),
                target: self.dev.name().into(),
            });
        }
        DevNodeSet::new(nodes)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct RtcInodeOps {
    dev: Arc<RtcDevice>,
}

impl InodeOps for RtcInodeOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if !self.dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        Ok(Box::new(RtcFileOps {
            dev: Arc::clone(&self.dev),
            cred: cred.clone(),
        }))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct RtcFileOps {
    dev: Arc<RtcDevice>,
    cred: Credentials,
}

impl RtcFileOps {
    fn require_sys_admin(&self) -> Result<(), Errno> {
        if self.cred.has_cap(Capability::SysAdmin) {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    }

    fn read_alarm(&self) -> Result<RtcAlarm, Errno> {
        match self.dev.control(RtcControlRequest::ReadAlarm)? {
            RtcControlResponse::Alarm(alarm) => Ok(alarm),
            _ => Err(Errno::EINVAL),
        }
    }
}

impl FileOps for RtcFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.len() < RTC_READ_WORD_LEN {
            return Err(VfsError::InvalidArgument);
        }
        // `/dev/rtc*` 的 read 语义是读取下一条 pending IRQ word。没有 pending
        // 事件时这里只返回 WouldBlock；是否阻塞、何时睡眠和何时重试由 syscall
        // 层根据 O_NONBLOCK 与 poll_add_waiter 统一处理，设备驱动不在这里自旋。
        let data = match self
            .dev
            .control(RtcControlRequest::ReadIrqData)
            .map_err(RtcError::to_vfs)?
        {
            RtcControlResponse::IrqData(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };
        let word = data.linux_word().to_le_bytes();
        buf[..RTC_READ_WORD_LEN].copy_from_slice(&word[..RTC_READ_WORD_LEN]);
        Ok(RTC_READ_WORD_LEN)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::NotSupported)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, interest: PollEvents) -> PollEvents {
        let always_report = PollEvents::POLLERR.with(PollEvents::POLLHUP);
        if !self.dev.is_active() {
            return always_report;
        }
        let ready = if self.dev.irq_ready() {
            PollEvents::POLLIN
        } else {
            PollEvents(0)
        };
        ready.intersect(interest.with(always_report))
    }

    fn poll_add_waiter(&self, task: &Arc<Task>, interest: PollEvents) -> bool {
        if interest.has(PollEvents::POLLIN) || interest.has(PollEvents::POLLPRI) {
            self.dev.add_irq_waiter(task);
            true
        } else {
            false
        }
    }

    fn poll_remove_waiter(&self, task: &Arc<Task>) {
        self.dev.remove_irq_waiter(task);
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        if !self.dev.is_active() {
            return Err(Errno::ENODEV);
        }

        match cmd.raw() {
            RTC_RD_TIME => {
                let time = match self.dev.control(RtcControlRequest::ReadTime)? {
                    RtcControlResponse::Time(time) => time,
                    _ => return Err(Errno::EINVAL),
                };
                write_rtc_time(arg, time)?;
                Ok(0)
            }
            RTC_SET_TIME => {
                // 当前能力模型还没有细分的时间设置权限，先用 SysAdmin 表达
                // “允许修改硬件/系统全局时间源”的权限边界。
                self.require_sys_admin()?;
                let time = read_rtc_time(arg)?;
                self.dev.control(RtcControlRequest::SetTime(time))?;
                Ok(0)
            }
            RTC_ALM_READ => {
                write_rtc_time(arg, self.read_alarm()?.time)?;
                Ok(0)
            }
            RTC_ALM_SET => {
                let base = match self.dev.control(RtcControlRequest::ReadTime)? {
                    RtcControlResponse::Time(time) => time,
                    _ => return Err(Errno::EINVAL),
                };
                let time = read_rtc_alarm_time(arg, base)?;
                let enabled = self
                    .read_alarm()
                    .map(|alarm| alarm.enabled)
                    .unwrap_or(false);
                self.dev.control(RtcControlRequest::SetAlarm(RtcAlarm {
                    time,
                    enabled,
                    pending: false,
                }))?;
                Ok(0)
            }
            RTC_WKALM_RD => {
                write_rtc_wkalrm(arg, self.read_alarm()?)?;
                Ok(0)
            }
            RTC_WKALM_SET => {
                let alarm = read_rtc_wkalrm(arg)?;
                self.dev.control(RtcControlRequest::SetAlarm(alarm))?;
                Ok(0)
            }
            RTC_AIE_ON => {
                self.dev
                    .control(RtcControlRequest::SetAlarmIrqEnabled(true))?;
                Ok(0)
            }
            RTC_AIE_OFF => {
                self.dev
                    .control(RtcControlRequest::SetAlarmIrqEnabled(false))?;
                Ok(0)
            }
            RTC_UIE_ON => {
                self.dev
                    .control(RtcControlRequest::SetUpdateIrqEnabled(true))?;
                Ok(0)
            }
            RTC_UIE_OFF => {
                self.dev
                    .control(RtcControlRequest::SetUpdateIrqEnabled(false))?;
                Ok(0)
            }
            RTC_PIE_ON => {
                self.dev
                    .control(RtcControlRequest::SetPeriodicIrqEnabled(true))?;
                Ok(0)
            }
            RTC_PIE_OFF => {
                self.dev
                    .control(RtcControlRequest::SetPeriodicIrqEnabled(false))?;
                Ok(0)
            }
            RTC_IRQP_READ => {
                let hz = match self.dev.control(RtcControlRequest::ReadPeriodicRate)? {
                    RtcControlResponse::U32(hz) => hz,
                    _ => return Err(Errno::EINVAL),
                };
                write_user_usize(arg, hz as usize)?;
                Ok(0)
            }
            RTC_IRQP_SET => {
                self.require_sys_admin()?;
                let hz = read_user_usize(arg)?;
                let hz = u32::try_from(hz).map_err(|_| Errno::EINVAL)?;
                self.dev.control(RtcControlRequest::SetPeriodicRate(hz))?;
                Ok(0)
            }
            RTC_EPOCH_READ => {
                let epoch = match self.dev.control(RtcControlRequest::ReadEpoch)? {
                    RtcControlResponse::U32(epoch) => epoch,
                    _ => return Err(Errno::EINVAL),
                };
                write_user_usize(arg, epoch as usize)?;
                Ok(0)
            }
            RTC_EPOCH_SET => {
                self.require_sys_admin()?;
                let epoch = read_user_usize(arg)?;
                let epoch = u32::try_from(epoch).map_err(|_| Errno::EINVAL)?;
                self.dev.control(RtcControlRequest::SetEpoch(epoch))?;
                Ok(0)
            }
            RTC_VL_READ => {
                let flags = match self.dev.control(RtcControlRequest::ReadVoltageLow)? {
                    RtcControlResponse::U32(flags) => flags,
                    _ => return Err(Errno::EINVAL),
                };
                write_user_u32(arg, flags)?;
                Ok(0)
            }
            RTC_VL_CLR => {
                self.require_sys_admin()?;
                self.dev.control(RtcControlRequest::ClearVoltageLow)?;
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl From<RtcError> for Errno {
    fn from(value: RtcError) -> Self {
        value.to_errno()
    }
}

fn read_rtc_time(user: usize) -> Result<RtcDateTime, Errno> {
    let mut raw = [0u8; RTC_TIME_LEN];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    read_rtc_time_from_bytes(&raw)
}

fn read_rtc_alarm_time(user: usize, base: RtcDateTime) -> Result<RtcDateTime, Errno> {
    let mut raw = [0u8; RTC_TIME_LEN];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let sec = get_i32(&raw, 0)?;
    let min = get_i32(&raw, 4)?;
    let hour = get_i32(&raw, 8)?;
    if sec < 0 || min < 0 || hour < 0 {
        return Err(Errno::EINVAL);
    }
    // 旧式 RTC_ALM_SET 只定义时/分/秒；部分用户态会把日期字段留空。
    // 这里按“下一次出现的 h:m:s”补齐日期，完整日期 alarm 则通过 RTC_WKALM_SET
    // 表达。若目标时刻已经不晚于当前 RTC 时间，自动滚到下一天，避免把硬件
    // alarm 编程到过去导致 read() 永远等不到事件。
    let target = RtcDateTime::new(
        base.year,
        base.month,
        base.day,
        u32::try_from(hour).map_err(|_| Errno::EINVAL)?,
        u32::try_from(min).map_err(|_| Errno::EINVAL)?,
        u32::try_from(sec).map_err(|_| Errno::EINVAL)?,
    )
    .ok_or(Errno::EINVAL)?;
    if target.unix_time_ns().ok_or(Errno::EINVAL)? <= base.unix_time_ns().ok_or(Errno::EINVAL)? {
        let next = target
            .unix_time_ns()
            .and_then(|ns| ns.checked_add(SECS_PER_DAY.checked_mul(NSEC_PER_SEC)?))
            .and_then(RtcDateTime::from_unix_time_ns)
            .ok_or(Errno::EINVAL)?;
        return Ok(next);
    }
    Ok(target)
}

fn read_rtc_time_from_bytes(raw: &[u8]) -> Result<RtcDateTime, Errno> {
    let sec = get_i32(raw, 0)?;
    let min = get_i32(raw, 4)?;
    let hour = get_i32(raw, 8)?;
    let mday = get_i32(raw, 12)?;
    let mon = get_i32(raw, 16)?;
    let year = get_i32(raw, 20)?;
    if sec < 0 || min < 0 || hour < 0 || mday <= 0 || mon < 0 || year < 70 {
        return Err(Errno::EINVAL);
    }
    RtcDateTime::new(
        u32::try_from(year + 1900).map_err(|_| Errno::EINVAL)?,
        u32::try_from(mon + 1).map_err(|_| Errno::EINVAL)?,
        u32::try_from(mday).map_err(|_| Errno::EINVAL)?,
        u32::try_from(hour).map_err(|_| Errno::EINVAL)?,
        u32::try_from(min).map_err(|_| Errno::EINVAL)?,
        u32::try_from(sec).map_err(|_| Errno::EINVAL)?,
    )
    .ok_or(Errno::EINVAL)
}

fn write_rtc_time_to_bytes(raw: &mut [u8], time: RtcDateTime) -> Result<(), Errno> {
    put_i32(raw, 0, time.second as i32);
    put_i32(raw, 4, time.minute as i32);
    put_i32(raw, 8, time.hour as i32);
    put_i32(raw, 12, time.day as i32);
    put_i32(raw, 16, time.month as i32 - 1);
    put_i32(raw, 20, time.year as i32 - 1900);
    put_i32(raw, 24, time.weekday().ok_or(Errno::EINVAL)? as i32);
    put_i32(raw, 28, time.yearday0().ok_or(Errno::EINVAL)? as i32);
    put_i32(raw, 32, 0);
    Ok(())
}

fn write_rtc_time(user: usize, time: RtcDateTime) -> Result<(), Errno> {
    let mut raw = [0u8; RTC_TIME_LEN];
    write_rtc_time_to_bytes(&mut raw, time)?;
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn read_rtc_wkalrm(user: usize) -> Result<RtcAlarm, Errno> {
    let mut raw = [0u8; RTC_WKALRM_LEN];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    let enabled = raw[0] != 0;
    let pending = raw[1] != 0;
    let time = read_rtc_time_from_bytes(&raw[4..4 + RTC_TIME_LEN])?;
    Ok(RtcAlarm {
        time,
        enabled,
        pending,
    })
}

fn write_rtc_wkalrm(user: usize, alarm: RtcAlarm) -> Result<(), Errno> {
    let mut raw = [0u8; RTC_WKALRM_LEN];
    raw[0] = u8::from(alarm.enabled);
    raw[1] = u8::from(alarm.pending);
    write_rtc_time_to_bytes(&mut raw[4..4 + RTC_TIME_LEN], alarm.time)?;
    copy_to_user(user, &raw).map_err(|e| e.as_errno())
}

fn read_user_usize(user: usize) -> Result<usize, Errno> {
    let mut raw = [0u8; core::mem::size_of::<usize>()];
    copy_from_user(user, &mut raw).map_err(|e| e.as_errno())?;
    Ok(usize::from_le_bytes(raw))
}

fn write_user_usize(user: usize, value: usize) -> Result<(), Errno> {
    copy_to_user(user, &value.to_le_bytes()).map_err(|e| e.as_errno())
}

fn write_user_u32(user: usize, value: u32) -> Result<(), Errno> {
    copy_to_user(user, &value.to_le_bytes()).map_err(|e| e.as_errno())
}

fn get_i32(raw: &[u8], offset: usize) -> Result<i32, Errno> {
    let bytes = raw.get(offset..offset + 4).ok_or(Errno::EINVAL)?;
    Ok(i32::from_le_bytes(bytes.try_into().unwrap()))
}

fn put_i32(raw: &mut [u8], offset: usize, value: i32) {
    raw[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn validate_periodic_rate(hz: u32) -> Result<(), RtcError> {
    if !(RTC_MIN_PERIODIC_RATE_HZ..=RTC_MAX_PERIODIC_RATE_HZ).contains(&hz) || !hz.is_power_of_two()
    {
        return Err(RtcError::Invalid);
    }
    Ok(())
}

fn days_in_month(year: u32, month: u32) -> Option<u32> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 => Some(if is_leap_year(year) { 29 } else { 28 }),
        _ => None,
    }
}

fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}
