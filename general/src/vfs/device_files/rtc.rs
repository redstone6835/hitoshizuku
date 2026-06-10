//! RTC devtmpfs 兼容节点适配层。
//!
//! 本模块位于 VFS/ABI 边界：把 `/dev/rtc*` 的 ioctl number、用户态
//! `struct rtc_time` / `struct rtc_wkalrm` 布局和用户指针拷贝，翻译成
//! [`crate::dev::rtc`] 的 typed control。底层 RTC 设备抽象只暴露时间、
//! alarm、IRQ 等语义，不承载用户 ABI 细节。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::any::Any;
use core::ops::ControlFlow;

use errno::Errno;
use sched::Task;
use vfs::cred::{Capability, Credentials};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeOps};

use crate::dev::rtc::{
    RtcAlarm, RtcControlRequest, RtcControlResponse, RtcDateTime, RtcDevice, RtcError,
    RtcIrqData, RtcIrqFlags,
};
use crate::vfs::device_files::spec::CustomDevNodeSpec;
use crate::vfs::devtmpfs::{
    DevTmpfsCustomNodeAdapter, DevTmpfsCustomNodeAdapterRegistration,
    register_custom_devnode_adapter,
};
use crate::vfs::user_api::ioctl::{
    read_bytes_from_user, write_bytes_to_user, write_u32_to_user, write_usize_to_user,
};

const NSEC_PER_SEC: u64 = 1_000_000_000;
const SECS_PER_DAY: u64 = 86_400;
const RTC_TIME_LEN: usize = 9 * core::mem::size_of::<i32>();
const RTC_WKALRM_LEN: usize = 4 + RTC_TIME_LEN;
const RTC_READ_WORD_LEN: usize = core::mem::size_of::<usize>();

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

const RTC_DEVNODE_ADAPTER_OWNER: &str = "rtc-devnode";
const RTC_DEVNODE_ADAPTER_NAME: &str = "rtc";

/// RTC 在 devtmpfs 自定义节点中的 typed endpoint。
///
/// 这里不保存 VFS inode/file 操作，也不保存 ioctl number。VFS 投影层只用它把
/// `RtcFunction` 中的 typed 设备传给 devtmpfs custom adapter，避免底层 RTC
/// 设备抽象直接依赖用户 ABI 或 inode 类型。
#[derive(Clone)]
pub struct RtcDevNodeEndpoint {
    dev: Arc<RtcDevice>,
}

impl RtcDevNodeEndpoint {
    pub fn new(dev: Arc<RtcDevice>) -> Self {
        Self { dev }
    }

    pub fn dev(&self) -> Arc<RtcDevice> {
        Arc::clone(&self.dev)
    }
}

/// 注册 RTC custom devnode 适配器。
///
/// 启动期 VFS 兼容层调用本函数，把 [`RtcDevNodeEndpoint`] 的解释能力挂到
/// devtmpfs adapter registry。devtmpfs 核心因此不需要直接依赖 RTC 类型。
pub fn register_devtmpfs_adapter() -> VfsResult<DevTmpfsCustomNodeAdapterRegistration> {
    register_custom_devnode_adapter(DevTmpfsCustomNodeAdapter::new(
        RTC_DEVNODE_ADAPTER_OWNER,
        RTC_DEVNODE_ADAPTER_NAME,
        build_rtc_inode_ops,
    ))
}

fn build_rtc_inode_ops(
    spec: &CustomDevNodeSpec,
) -> VfsResult<Option<Arc<dyn InodeOps + Send + Sync>>> {
    let payload = spec.payload();
    let Some(endpoint) = payload.as_ref().downcast_ref::<RtcDevNodeEndpoint>() else {
        return Ok(None);
    };
    Ok(Some(inode_ops(endpoint)))
}

/// 根据 dev core 提供的 typed endpoint 构造 RTC inode 操作对象。
///
/// 这里是 RTC opaque payload 的唯一解释点；新增 RTC 硬件驱动不会影响
/// devtmpfs 的分发逻辑，只要继续发布 [`RtcDevNodeEndpoint`] 即可。
pub fn inode_ops(endpoint: &RtcDevNodeEndpoint) -> Arc<dyn InodeOps + Send + Sync> {
    Arc::new(RtcInodeOps {
        dev: endpoint.dev(),
    })
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
            .map_err(map_rtc_vfs_error)?
        {
            RtcControlResponse::IrqData(data) => data,
            _ => return Err(VfsError::InvalidArgument),
        };
        let word = linux_irq_word(data).to_le_bytes();
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
        map_rtc_errno(value)
    }
}

fn map_rtc_errno(err: RtcError) -> Errno {
    match err {
        RtcError::Unsupported => Errno::ENOTTY,
        RtcError::Invalid => Errno::EINVAL,
        RtcError::NoDevice => Errno::ENODEV,
        RtcError::Busy => Errno::EBUSY,
        RtcError::WouldBlock => Errno::EAGAIN,
        RtcError::Io => Errno::EIO,
        RtcError::Permission => Errno::EPERM,
    }
}

fn map_rtc_vfs_error(err: RtcError) -> VfsError {
    match err {
        RtcError::Unsupported => VfsError::NotSupported,
        RtcError::Invalid => VfsError::InvalidArgument,
        RtcError::NoDevice => VfsError::NoDevice,
        RtcError::Busy => VfsError::DeviceBusy,
        RtcError::WouldBlock => VfsError::WouldBlock,
        RtcError::Io => VfsError::Io,
        RtcError::Permission => VfsError::OperationNotPermitted,
    }
}

fn linux_irq_word(data: RtcIrqData) -> usize {
    let mut flags = 0u32;
    if data.flags.contains(RtcIrqFlags::PERIODIC) {
        flags |= RTC_IRQF | RTC_PF;
    }
    if data.flags.contains(RtcIrqFlags::ALARM) {
        flags |= RTC_IRQF | RTC_AF;
    }
    if data.flags.contains(RtcIrqFlags::UPDATE) {
        flags |= RTC_IRQF | RTC_UF;
    }
    ((data.count as usize) << 8) | flags as usize
}

fn read_rtc_time(user: usize) -> Result<RtcDateTime, Errno> {
    let mut raw = [0u8; RTC_TIME_LEN];
    read_bytes_from_user(user, &mut raw)?;
    read_rtc_time_from_bytes(&raw)
}

fn read_rtc_alarm_time(user: usize, base: RtcDateTime) -> Result<RtcDateTime, Errno> {
    let mut raw = [0u8; RTC_TIME_LEN];
    read_bytes_from_user(user, &mut raw)?;
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
    write_bytes_to_user(user, &raw)
}

fn read_rtc_wkalrm(user: usize) -> Result<RtcAlarm, Errno> {
    let mut raw = [0u8; RTC_WKALRM_LEN];
    read_bytes_from_user(user, &mut raw)?;
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
    write_bytes_to_user(user, &raw)
}

fn read_user_usize(user: usize) -> Result<usize, Errno> {
    let mut raw = [0u8; core::mem::size_of::<usize>()];
    read_bytes_from_user(user, &mut raw)?;
    Ok(usize::from_le_bytes(raw))
}

fn write_user_usize(user: usize, value: usize) -> Result<(), Errno> {
    write_usize_to_user(user, value)
}

fn write_user_u32(user: usize, value: u32) -> Result<(), Errno> {
    write_u32_to_user(user, value)
}

fn get_i32(raw: &[u8], offset: usize) -> Result<i32, Errno> {
    let bytes = raw.get(offset..offset + 4).ok_or(Errno::EINVAL)?;
    let mut out = [0u8; core::mem::size_of::<i32>()];
    out.copy_from_slice(bytes);
    Ok(i32::from_le_bytes(out))
}

fn put_i32(raw: &mut [u8], offset: usize, value: i32) {
    raw[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
