//! 虚拟终端(VT)。
//!
//! 与 Linux 的 `drivers/tty/vt` 对应但保持本内核的投影式设计:每个 VT 是
//! 一个独立的 [`TtyCore`](crate::dev::tty::core::TtyCore) 实例(自己的
//! termios/前台进程组),后端 [`VtDevice`] 决定输出去向(活动 VT 写物理控制台,
//! 非活动 VT 写内存滚动缓冲)与输入来源(串口泵注入)。
//!
//! 设备号仍是呈现层产物:`ttyN` 节点由 well-known 策略映射为 `4:N`,不参与
//! 底层设备身份。VT 切换通过 `VT_ACTIVATE`/`VT_WAITACTIVE` 等 ioctl 完成;
//! 串口场景没有键盘,切换入口与 Linux 完全一致(openvt/chvt)。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use errno::Errno;
use sched::operation;
use vfs::file::IoctlCmd;
use vfs::sync::Spinlock;

use crate::dev::char::{CharDevice, CharIoError};
use crate::dev::control::{CharControlRequest, CharControlResponse, ControlError};
use crate::dev::tty::core::{
    TerminalDriver, TtyControlRequest, TtyControlResponse, TtyIoError, TtyIoResult, lookup_tty_core,
};
use crate::vfs::user_api::ioctl::{read_bytes_from_user, write_bytes_to_user, write_i32_to_user};
use crate::vfs::user_api::tty::TtyIoctlState;

/// 虚拟终端数量(含 tty0 别名):tty1..=tty7。
pub const VT_COUNT: usize = 8;

/// 非活动 VT 的屏幕滚动缓冲上限。
const SCROLLBACK_CAP: usize = 64 * 1024;

// ── VT/KD 模式常量(与 Linux vt.h/kd.h 一致) ─────────────────────────────────

/// VT_AUTO:内核自动管理切换。
pub const VT_AUTO: u8 = 0;
/// VT_PROCESS:用户进程(如 X)接管切换,需要 VT_RELDISP 配合。
pub const VT_PROCESS: u8 = 1;
/// VT_HARDWARE:硬件管理(本内核不支持,仅存储)。
pub const VT_HARDWARE: u8 = 2;

pub const KD_TEXT: u8 = 0x00;
pub const KD_GRAPHICS: u8 = 0x01;

pub const K_RAW: u8 = 0x00;
pub const K_XLATE: u8 = 0x01;
pub const K_MEDIUMRAW: u8 = 0x02;
pub const K_OFF: u8 = 0x03;
pub const KB_101: u8 = 0x02;

// ── ioctl 号(与 Linux 一致) ─────────────────────────────────────────────────

const VT_OPENQRY: usize = 0x5600;
const VT_GETMODE: usize = 0x5601;
const VT_SETMODE: usize = 0x5602;
const VT_GETSTATE: usize = 0x5603;
const VT_SENDSIG: usize = 0x5604;
const VT_RELDISP: usize = 0x5605;
const VT_ACTIVATE: usize = 0x5606;
const VT_WAITACTIVE: usize = 0x5607;
const VT_DISALLOCATE: usize = 0x5608;
const VT_RESIZE: usize = 0x5609;
const VT_LOCKSWITCH: usize = 0x560b;
const VT_UNLOCKSWITCH: usize = 0x560c;

const KDSETMODE: usize = 0x4b3a;
const KDGETMODE: usize = 0x4b3b;
const KDSKBMODE: usize = 0x4b45;
const KDGKBMODE: usize = 0x4b44;
const KDGKBTYPE: usize = 0x4b33;
const KDSIGACCEPT: usize = 0x4b4e;

/// 单个 VT 的可变状态。
struct VtState {
    input: VecDeque<u8>,
    screen: Vec<u8>,
    graphics: bool,
    mode: u8,
    relsig: u16,
    acqsig: u16,
    frsig: u16,
    /// VT_PROCESS 持有者 pid(0 表示无)。
    owner: i32,
    /// VT_PROCESS 持有者是否已释放显示(VT_RELDISP(1))。
    released: bool,
    keyboard_mode: u8,
    /// KDSIGACCEPT 接受的信号。
    accept_sig: i32,
}

impl VtState {
    fn new() -> Self {
        Self {
            input: VecDeque::new(),
            screen: Vec::new(),
            graphics: false,
            mode: VT_AUTO,
            relsig: 0,
            acqsig: 0,
            frsig: 0,
            owner: 0,
            released: true,
            keyboard_mode: K_XLATE,
            accept_sig: 0,
        }
    }
}

/// 一个虚拟终端。
pub struct VtDevice {
    index: u8,
    manager: &'static VtManager,
    state: Spinlock<VtState>,
    open_count: AtomicU32,
    /// 绑定到 devtmpfs 的字符设备(install 时创建)。
    dev: Spinlock<Option<CharDevice>>,
}

impl VtDevice {
    fn new(index: u8, manager: &'static VtManager) -> Self {
        Self {
            index,
            manager,
            state: Spinlock::new(VtState::new()),
            open_count: AtomicU32::new(0),
            dev: Spinlock::new(None),
        }
    }

    pub fn index(&self) -> u8 {
        self.index
    }

    /// 该 VT 的 devtmpfs 节点名(`ttyN`)。
    pub fn name(&self) -> String {
        let mut out = String::new();
        out.try_reserve(8).ok();
        out.push_str("tty");
        out.push_str(&self.index.to_string());
        out
    }

    /// 该 VT 绑定的字符设备。
    pub fn char_device(&self) -> Option<CharDevice> {
        self.dev.lock().clone()
    }

    pub fn is_fg(&self) -> bool {
        self.manager.fg.load(Ordering::Acquire) == self.index
    }

    fn manager(&self) -> &'static VtManager {
        self.manager
    }

    /// 打开计数(open 时 +1,release 时 -1;VT_OPENQRY 使用)。
    pub fn note_open(&self, delta: i32) {
        let count = self.open_count.load(Ordering::Acquire) as i64;
        let next = (count + i64::from(delta)).clamp(0, i64::from(u32::MAX)) as u32;
        self.open_count.store(next, Ordering::Release);
    }

    pub fn is_open(&self) -> bool {
        self.open_count.load(Ordering::Acquire) != 0
    }

    /// 串口泵注入输入字节。
    pub fn inject_input(&self, bytes: &[u8]) {
        let mut state = self.state.lock();
        for &byte in bytes {
            if state.input.len() >= SCROLLBACK_CAP {
                break;
            }
            state.input.push_back(byte);
        }
    }

    /// 把输出写入活动控制台(活动 VT 且 VT 为活动控制台时)或滚动缓冲。
    ///
    /// `console=ttyN` 时活动 VT 输出镜像到物理控制台(VT 屏幕投影到串口);
    /// `console=uart0` 等非 VT 控制台下 VT 与 Linux 无头场景一致:输出只进
    /// 内存滚动缓冲,不泄漏到物理串口。
    fn route_output(&self, buf: &[u8]) -> TtyIoResult<usize> {
        if self.is_fg() && self.manager.vt_console.load(Ordering::Acquire) {
            let console = self
                .manager
                .console
                .lock()
                .clone()
                .ok_or(TtyIoError::NoDevice)?;
            return console.write(buf).map_err(map_char_err);
        }
        let mut state = self.state.lock();
        let remaining = SCROLLBACK_CAP.saturating_sub(state.screen.len());
        let n = buf.len().min(remaining);
        state.screen.extend_from_slice(&buf[..n]);
        Ok(n)
    }

    fn route_output_all(&self, buf: &[u8]) -> TtyIoResult<()> {
        if self.is_fg() && self.manager.vt_console.load(Ordering::Acquire) {
            let console = self
                .manager
                .console
                .lock()
                .clone()
                .ok_or(TtyIoError::NoDevice)?;
            return console.write_all(buf).map_err(map_char_err);
        }
        let mut state = self.state.lock();
        let remaining = SCROLLBACK_CAP.saturating_sub(state.screen.len());
        let n = buf.len().min(remaining);
        state.screen.extend_from_slice(&buf[..n]);
        Ok(())
    }

    /// VT_PROCESS 语义:是否需要等待持有者 VT_RELDISP 才能切出。
    fn needs_release_wait(&self) -> bool {
        let state = self.state.lock();
        state.mode == VT_PROCESS
            && !state.released
            && state.owner > 0
            && operation::getpgid(state.owner).is_ok()
    }

    /// 提醒 VT_PROCESS 持有者释放显示(发 relsig)。
    fn signal_release(&self) {
        let (owner, relsig) = {
            let state = self.state.lock();
            (state.owner, state.relsig)
        };
        if owner > 0 && relsig != 0 {
            let _ = operation::kill(owner, sched::SignalNumber::from_raw(i32::from(relsig)));
        }
    }

    /// 切换到本 VT 完成后通知持有者(发 acqsig)。
    fn notify_acquired(&self) {
        let (owner, acqsig) = {
            let state = self.state.lock();
            (state.owner, state.acqsig)
        };
        if owner > 0 && acqsig != 0 {
            let _ = operation::kill(owner, sched::SignalNumber::from_raw(i32::from(acqsig)));
        }
    }
}

impl TerminalDriver for VtDevice {
    fn write_output(&self, buf: &[u8]) -> TtyIoResult<usize> {
        self.route_output(buf)
    }

    fn write_all_output(&self, buf: &[u8]) -> TtyIoResult<()> {
        self.route_output_all(buf)
    }

    fn read_input(&self, buf: &mut [u8]) -> TtyIoResult<usize> {
        let mut state = self.state.lock();
        let mut n = 0usize;
        while n < buf.len() {
            let Some(byte) = state.input.pop_front() else {
                break;
            };
            buf[n] = byte;
            n += 1;
        }
        Ok(n)
    }

    fn poll_read(&self) -> bool {
        !self.state.lock().input.is_empty()
    }

    fn is_active(&self) -> bool {
        true
    }

    fn control(&self, req: TtyControlRequest) -> TtyIoResult<TtyControlResponse> {
        match req {
            TtyControlRequest::DrainTx | TtyControlRequest::FlushTx => Ok(TtyControlResponse::Done),
            TtyControlRequest::FlushRx | TtyControlRequest::FlushBoth => {
                self.state.lock().input.clear();
                Ok(TtyControlResponse::Done)
            }
            TtyControlRequest::GetInputQueueLen => {
                Ok(TtyControlResponse::U32(self.state.lock().input.len() as u32))
            }
            TtyControlRequest::GetOutputQueueLen => Ok(TtyControlResponse::U32(
                self.state.lock().screen.len() as u32,
            )),
            TtyControlRequest::SetSerialConfig { .. } | TtyControlRequest::SendBreak { .. } => {
                Err(TtyIoError::Unsupported)
            }
        }
    }

    fn activate(&self, _active: bool) {}

    fn hangup(&self) {}
}

// ── CharDriver 适配:VT 通过 devtmpfs 字符设备节点暴露 ────────────────────────

/// 把 [`VtDevice`] 适配为 [`CharDriver`],供 devtmpfs 节点绑定。
pub struct VtCharDriver {
    vt: Arc<VtDevice>,
}

impl VtCharDriver {
    pub fn vt(&self) -> &Arc<VtDevice> {
        &self.vt
    }
}

impl crate::dev::char::CharDriver for VtCharDriver {
    fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
        self.vt.write_output(buf).map_err(map_tty_err)
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        self.vt.read_input(buf).map_err(map_tty_err)
    }

    fn flush(&self) -> Result<(), CharIoError> {
        Ok(())
    }

    fn poll_read(&self) -> bool {
        self.vt.poll_read()
    }

    fn is_tty(&self) -> bool {
        true
    }

    fn control(&self, req: CharControlRequest) -> Result<CharControlResponse, ControlError> {
        let tty_req = match req {
            CharControlRequest::DrainTx => TtyControlRequest::DrainTx,
            CharControlRequest::FlushTx => TtyControlRequest::FlushTx,
            CharControlRequest::FlushRx => TtyControlRequest::FlushRx,
            CharControlRequest::FlushBoth => TtyControlRequest::FlushBoth,
            CharControlRequest::SetSerialConfig { baud } => {
                TtyControlRequest::SetSerialConfig { baud }
            }
            CharControlRequest::SendBreak { duration_ms } => {
                TtyControlRequest::SendBreak { duration_ms }
            }
            CharControlRequest::GetInputQueueLen => TtyControlRequest::GetInputQueueLen,
            CharControlRequest::GetOutputQueueLen => TtyControlRequest::GetOutputQueueLen,
        };
        match self.vt.control(tty_req) {
            Ok(TtyControlResponse::Done) => Ok(CharControlResponse::Done),
            Ok(TtyControlResponse::U32(value)) => Ok(CharControlResponse::U32(value)),
            Err(TtyIoError::Unsupported) => Err(ControlError::Unsupported),
            Err(TtyIoError::Invalid) => Err(ControlError::Invalid),
            Err(TtyIoError::NoDevice) => Err(ControlError::NoDevice),
            Err(TtyIoError::Busy) => Err(ControlError::Busy),
            Err(TtyIoError::Io) => Err(ControlError::Io),
            Err(_) => Err(ControlError::Invalid),
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── 管理器 ────────────────────────────────────────────────────────────────────

/// VT 管理器(单例,生命周期等同内核)。
pub struct VtManager {
    vts: Spinlock<Vec<Arc<VtDevice>>>,
    fg: AtomicU8,
    console: Spinlock<Option<CharDevice>>,
    /// VT 是否作为活动控制台(`console=ttyN` 时置位):置位时串口输入路由到
    /// 活动 VT,活动 VT 输出镜像到物理控制台;否则 VT 仅存在于内存(输入由
    /// 原串口行规程消费,输出进滚动缓冲),与 Linux 无头 VT 行为一致。
    vt_console: AtomicBool,
    switch_lock: Spinlock<()>,
    /// VT_LOCKSWITCH 置位后禁止切换。
    locked: AtomicBool,
}

static MANAGER: Spinlock<Option<&'static VtManager>> = Spinlock::new(None);

impl VtManager {
    /// 安装 VT 管理器(幂等),返回单例。
    ///
    /// `route_input` 为 true 时 VT 作为活动控制台(`console=tty0` 等场景):
    /// 串口泵把物理控制台输入路由到活动 VT,活动 VT 输出镜像到物理控制台;
    /// 否则 VT 仍存在且可经 ioctl 切换,但输入保持由原串口行规程消费,
    /// 输出只进内存滚动缓冲。
    pub fn install(console: CharDevice, route_input: bool) -> &'static VtManager {
        let mut slot = MANAGER.lock();
        if let Some(existing) = *slot {
            return existing;
        }
        let leaked: &'static VtManager = Box::leak(Box::new(VtManager {
            vts: Spinlock::new(Vec::new()),
            fg: AtomicU8::new(1),
            console: Spinlock::new(Some(console.clone())),
            vt_console: AtomicBool::new(route_input),
            switch_lock: Spinlock::new(()),
            locked: AtomicBool::new(false),
        }));
        // VT 作为活动控制台时,物理控制台输入归 VT 泵所有:行规程的用户读
        // 路径不得再拉取同一 FIFO(见 CharDeviceTerminalDriver::read_input)。
        crate::dev::tty::core::set_vt_console_input_owner(if route_input {
            Some(console.fw_name())
        } else {
            None
        });
        let mut vts = leaked.vts.lock();
        for index in 1..VT_COUNT {
            let vt = Arc::new(VtDevice::new(index as u8, leaked));
            let driver: Arc<dyn crate::dev::char::CharDriver> = Arc::new(VtCharDriver {
                vt: Arc::clone(&vt),
            });
            let dev = CharDevice::from_arc(vt.name().into_boxed_str(), driver);
            vt.dev.lock().replace(dev);
            vts.push(vt);
        }
        drop(vts);
        *slot = Some(leaked);
        leaked
    }

    pub fn global() -> Option<&'static VtManager> {
        *MANAGER.lock()
    }

    /// 当前活动 VT 编号。
    pub fn fg_index(&self) -> u8 {
        self.fg.load(Ordering::Acquire)
    }

    /// 按编号取 VT(tty0 返回 None,它只是别名)。
    pub fn vt(&self, index: u8) -> Option<Arc<VtDevice>> {
        let index = usize::from(index);
        if index == 0 || index >= VT_COUNT {
            return None;
        }
        self.vts.lock().get(index - 1).cloned()
    }

    /// 当前活动 VT。
    pub fn fg_vt(&self) -> Option<Arc<VtDevice>> {
        self.vt(self.fg_index())
    }

    /// 物理控制台字符设备。
    pub fn console_device(&self) -> Option<CharDevice> {
        self.console.lock().clone()
    }

    /// 串口输入泵:读物理控制台字节 → 注入活动 VT → 驱动其行规程。
    ///
    /// 返回 true 表示本泵消费了控制台输入(调用方不应再走通用 TTY drain)。
    pub fn pump_console(&self) -> bool {
        if !self.vt_console.load(Ordering::Acquire) {
            return false;
        }
        let Some(console) = self.console.lock().clone() else {
            return false;
        };
        let Some(fg) = self.fg_vt() else {
            return false;
        };
        let mut buf = [0u8; 64];
        let mut total = 0usize;
        while total < 256 {
            let n = match console.read(&mut buf) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            fg.inject_input(&buf[..n]);
            total += n;
        }
        if let Some(core) = lookup_tty_core(&fg.name()) {
            let termios = core.termios();
            core.drain_tty_input(termios);
        }
        true
    }

    /// 切换到目标 VT。
    ///
    /// 若当前 VT 处于 VT_PROCESS 模式且持有者未释放显示,向持有者发 relsig
    /// 并等待其调用 VT_RELDISP(1)(或持有者退出),与 Linux 语义一致。
    pub fn activate(&self, target: u8) -> Result<(), Errno> {
        if self.vt(target).is_none() {
            return Err(Errno::ENODEV);
        }
        if self.locked.load(Ordering::Acquire) {
            return Err(Errno::EPERM);
        }
        loop {
            let current = self.fg.load(Ordering::Acquire);
            if current == target {
                return Ok(());
            }
            let need_release = self.vt(current).is_some_and(|vt| vt.needs_release_wait());
            if !need_release {
                break;
            }
            if let Some(vt) = self.vt(current) {
                vt.signal_release();
            }
            vt_wait_recheck();
        }
        self.do_activate(target);
        Ok(())
    }

    fn do_activate(&self, target: u8) {
        let _guard = self.switch_lock.lock();
        let old = self.fg.swap(target, Ordering::AcqRel);
        if old == target {
            return;
        }
        if let Some(vt) = self.vt(target) {
            vt.notify_acquired();
        }
    }
}

/// 记录一次 VT 节点打开(由 devtmpfs FileOps 构造时调用)。
pub fn note_vt_opened(dev: &CharDevice, delta: i32) {
    if let Some(vt) = vt_from_char_device(dev) {
        vt.note_open(delta);
    }
}

/// 等待一次 10ms 重检窗口(供 VT_WAITACTIVE / VT_PROCESS 释放等待使用)。
fn vt_wait_recheck() {
    let task = sched::current_task_direct();
    let deadline = sched::now_ns_direct().saturating_add(10_000_000);
    let _ = task.cas_state(sched::TaskState::Running, sched::TaskState::Sleeping);
    let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Sleeping);
    let armed = sched::register_sleep_deadline(&task, deadline);
    sched::schedule_once(sched::now_ns_direct());
    if armed {
        sched::cancel_sleep_deadline(&task);
    }
    let _ = task.cas_state(sched::TaskState::Sleeping, sched::TaskState::Running);
    let _ = task.cas_state(sched::TaskState::Runnable, sched::TaskState::Running);
}

fn map_char_err(e: CharIoError) -> TtyIoError {
    match e {
        CharIoError::NoSpace => TtyIoError::NoSpace,
        CharIoError::HardwareError => TtyIoError::Io,
        CharIoError::Unavailable => TtyIoError::NoDevice,
        CharIoError::Interrupted => TtyIoError::Interrupted,
        CharIoError::Timeout => TtyIoError::TimedOut,
    }
}

fn map_tty_err(e: TtyIoError) -> CharIoError {
    match e {
        TtyIoError::NoSpace => CharIoError::NoSpace,
        TtyIoError::WouldBlock | TtyIoError::TimedOut => CharIoError::Timeout,
        TtyIoError::Interrupted => CharIoError::Interrupted,
        TtyIoError::NoDevice => CharIoError::Unavailable,
        TtyIoError::Io | TtyIoError::Invalid | TtyIoError::Unsupported | TtyIoError::Busy => {
            CharIoError::HardwareError
        }
        TtyIoError::NoSpace => CharIoError::HardwareError,
    }
}

/// 从 devtmpfs 字符设备取回其 VT(非 VT 设备返回 None)。
pub fn vt_from_char_device(dev: &CharDevice) -> Option<Arc<VtDevice>> {
    let driver = dev.downcast_driver::<VtCharDriver>()?;
    Some(Arc::clone(driver.vt()))
}

// ── ioctl 处理 ────────────────────────────────────────────────────────────────

fn pack_vt_mode(mode: u8, waitv: u8, relsig: u16, acqsig: u16, frsig: u16, out: &mut [u8]) {
    out[0] = mode;
    out[1] = waitv;
    out[2..4].copy_from_slice(&relsig.to_le_bytes());
    out[4..6].copy_from_slice(&acqsig.to_le_bytes());
    out[6..8].copy_from_slice(&frsig.to_le_bytes());
}

/// 处理 VT/KD ioctl。
///
/// 返回 `Ok(None)` 表示不是 VT 命令(调用方继续走 TTY ioctl 表);
/// `Ok(Some(n))` 表示已处理。ioctl 号、参数语义与 Linux 一致。
pub fn handle_vt_ioctl(
    vt: &Arc<VtDevice>,
    cmd: IoctlCmd,
    arg: usize,
) -> Result<Option<usize>, Errno> {
    let manager = vt.manager();
    match cmd.raw() {
        VT_OPENQRY => {
            let vts = manager.vts.lock();
            let free = vts
                .iter()
                .find(|vt| !vt.is_open())
                .map(|vt| i32::from(vt.index()));
            drop(vts);
            let Some(free) = free else {
                return Err(Errno::ENODEV);
            };
            write_i32_to_user(arg, free)?;
            Ok(Some(0))
        }
        VT_GETMODE => {
            let state = vt.state.lock();
            let mut raw = [0u8; 8];
            pack_vt_mode(
                state.mode,
                0,
                state.relsig,
                state.acqsig,
                state.frsig,
                &mut raw,
            );
            write_bytes_to_user(arg, &raw)?;
            Ok(Some(0))
        }
        VT_SETMODE => {
            let mut raw = [0u8; 8];
            read_bytes_from_user(arg, &mut raw)?;
            let mode = raw[0];
            if !matches!(mode, VT_AUTO | VT_PROCESS | VT_HARDWARE) {
                return Err(Errno::EINVAL);
            }
            let relsig = u16::from_le_bytes([raw[2], raw[3]]);
            let acqsig = u16::from_le_bytes([raw[4], raw[5]]);
            let frsig = u16::from_le_bytes([raw[6], raw[7]]);
            let mut state = vt.state.lock();
            state.mode = mode;
            state.relsig = relsig;
            state.acqsig = acqsig;
            state.frsig = frsig;
            state.owner = operation::getpid();
            state.released = mode != VT_PROCESS;
            Ok(Some(0))
        }
        VT_GETSTATE => {
            let active = manager.fg_index();
            let mut raw = [0u8; 6];
            raw[0..2].copy_from_slice(&u16::from(active).to_le_bytes());
            raw[2..4].copy_from_slice(&0u16.to_le_bytes());
            raw[4..6].copy_from_slice(&0u16.to_le_bytes());
            write_bytes_to_user(arg, &raw)?;
            Ok(Some(0))
        }
        VT_SENDSIG => {
            let sig = arg as i32;
            let Some(sig) = sched::SignalNumber::from_raw(sig) else {
                return Err(Errno::EINVAL);
            };
            if let Some(core) = lookup_tty_core(&vt.name()) {
                let pgrp = core.foreground_pgrp();
                if pgrp > 0 {
                    let _ = operation::kill_process_group(pgrp, Some(sig));
                }
            }
            Ok(Some(0))
        }
        VT_RELDISP => match arg {
            1 => {
                let mut state = vt.state.lock();
                state.released = true;
                Ok(Some(0))
            }
            2 => {
                let mut state = vt.state.lock();
                state.released = false;
                Ok(Some(0))
            }
            _ => Err(Errno::EINVAL),
        },
        VT_ACTIVATE => {
            let target = arg as u8;
            manager.activate(target)?;
            Ok(Some(0))
        }
        VT_WAITACTIVE => {
            let target = arg as u8;
            if manager.vt(target).is_none() {
                return Err(Errno::ENODEV);
            }
            loop {
                if manager.fg_index() == target {
                    return Ok(Some(0));
                }
                vt_wait_recheck();
            }
        }
        VT_DISALLOCATE => {
            let target = arg as u8;
            let Some(vt) = manager.vt(target) else {
                return Err(Errno::ENODEV);
            };
            if vt.is_open() {
                return Err(Errno::EBUSY);
            }
            Ok(Some(0))
        }
        VT_RESIZE => {
            let mut raw = [0u8; 4];
            read_bytes_from_user(arg, &mut raw)?;
            let rows = u16::from_le_bytes([raw[0], raw[1]]);
            let cols = u16::from_le_bytes([raw[2], raw[3]]);
            if let Some(core) = lookup_tty_core(&vt.name()) {
                let mut winsize = core.winsize().to_bytes();
                winsize[0..2].copy_from_slice(&rows.to_le_bytes());
                winsize[2..4].copy_from_slice(&cols.to_le_bytes());
                core.set_winsize(crate::vfs::user_api::tty::UserWinSize::from_bytes(winsize));
            }
            Ok(Some(0))
        }
        VT_LOCKSWITCH => {
            manager.locked.store(true, Ordering::Release);
            Ok(Some(0))
        }
        VT_UNLOCKSWITCH => {
            manager.locked.store(false, Ordering::Release);
            Ok(Some(0))
        }
        KDSETMODE => match arg as u8 {
            KD_TEXT => {
                vt.state.lock().graphics = false;
                Ok(Some(0))
            }
            KD_GRAPHICS => {
                vt.state.lock().graphics = true;
                Ok(Some(0))
            }
            _ => Err(Errno::EINVAL),
        },
        KDGETMODE => {
            let graphics = vt.state.lock().graphics;
            write_i32_to_user(arg, i32::from(if graphics { KD_GRAPHICS } else { KD_TEXT }))?;
            Ok(Some(0))
        }
        KDSKBMODE => match arg as u8 {
            K_RAW | K_XLATE | K_MEDIUMRAW | K_OFF => {
                vt.state.lock().keyboard_mode = arg as u8;
                Ok(Some(0))
            }
            _ => Err(Errno::EINVAL),
        },
        KDGKBMODE => {
            let mode = vt.state.lock().keyboard_mode;
            write_i32_to_user(arg, i32::from(mode))?;
            Ok(Some(0))
        }
        KDGKBTYPE => {
            // Linux 的 KDGKBTYPE 只写 1 字节(char):busybox 的
            // get_console_fd_or_die 用 `char arg` 接收,写满 int 会踩坏
            // 调用者栈帧导致段错误。
            write_bytes_to_user(arg, &[KB_101])?;
            Ok(Some(0))
        }
        KDSIGACCEPT => {
            let sig = arg as i32;
            if sched::SignalNumber::from_raw(sig).is_none() {
                return Err(Errno::EINVAL);
            }
            vt.state.lock().accept_sig = sig;
            Ok(Some(0))
        }
        _ => Ok(None),
    }
}
