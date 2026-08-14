//! TTY 行规程核心与终端后端契约。
//!
//! 本模块从 devtmpfs 字符设备适配层迁出:行规程状态(termios、窗口大小、
//! 前台进程组、规范行缓冲)与 I/O 后端解耦,由 [`TerminalDriver`] 提供
//! 字节流与生命周期回调。这样 VT、pts 与串口共用同一套信号/编辑/回显语义,
//! 同时保持"设备身份在底层设备模型、dev_t 只是用户 ABI 投影"的设计。
//!
//! 迁移时逐行保留原有行为;VFS 投影层只负责把 [`TtyIoError`] 映射为
//! `VfsError` 并挂接 FileOps。

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, Ordering};
use alloc::vec::Vec;

use errno::Errno;
use sched::operation;
use vfs::sync::Spinlock;

use crate::dev::char::{CharDevice, CharIoError};
use crate::dev::control::{CharControlRequest, CharControlResponse, ControlError};
use crate::vfs::user_api::tty::{UserTermios, UserWinSize};

/// TTY 数据路径错误。
///
/// 与 VFS 错误解耦,由投影层(devtmpfs 的 FileOps 胶水)映射为 `VfsError`;
/// 后端驱动通过 [`TerminalDriver`] 返回本类型,不依赖具体文件系统语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtyIoError {
    /// 当前无数据/无空间,调用方应等待就绪后再试。
    WouldBlock,
    /// 输入触发控制字符信号(如 Ctrl-C),本次处理被信号打断。
    Interrupted,
    /// 内部缓冲扩容失败。
    NoSpace,
    /// 硬件级 I/O 错误。
    Io,
    /// 设备已不可用或断开。
    NoDevice,
    /// 自旋/等待超时。
    TimedOut,
    /// 后端不支持该控制请求(调用方按"无此能力"忽略)。
    Unsupported,
    /// 后端正忙。
    Busy,
    /// 无效请求或参数。
    Invalid,
}

pub type TtyIoResult<T> = Result<T, TtyIoError>;

/// 终端后端控制请求。
///
/// 与 `CharControlRequest` 语义一致,但后端不一定是 `CharDevice`
/// (pts 的 slave 后端就是环形缓冲)。串口适配层负责映射到硬件控制面。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtyControlRequest {
    /// 等待已提交输出全部发送完成。
    DrainTx,
    /// 丢弃尚未发送完成的输出队列。
    FlushTx,
    /// 丢弃尚未被上层消费的输入队列。
    FlushRx,
    /// 同时丢弃输入与输出队列。
    FlushBoth,
    /// 配置串口类硬件;`baud == None` 表示只同步其它行规程状态。
    SetSerialConfig {
        baud: Option<u32>,
    },
    /// 让发送线进入 break 条件并保持指定时长。
    SendBreak {
        duration_ms: u32,
    },
    GetInputQueueLen,
    GetOutputQueueLen,
}

/// 终端后端控制响应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtyControlResponse {
    Done,
    U32(u32),
}

/// 终端后端契约。
///
/// 行规程只通过本 trait 与终端交互,不感知具体设备。实现方负责并发保护;
/// 无对应能力的后端使用默认实现(空操作或 `Invalid`)。
pub trait TerminalDriver: Send + Sync {
    /// 尽量写入输出,返回实际接受字节数(`Ok(0)` 表示当前无法接受)。
    fn write_output(&self, buf: &[u8]) -> TtyIoResult<usize>;

    /// 阻塞式写全部输出。
    ///
    /// 默认逐次调用 [`Self::write_output`] 直到写完;遇到 `Ok(0)` 返回
    /// `WouldBlock`。串口等有硬件排空语义的后端可覆盖为设备级 `write_all`。
    fn write_all_output(&self, buf: &[u8]) -> TtyIoResult<()> {
        let mut remaining = buf;
        while !remaining.is_empty() {
            match self.write_output(remaining) {
                Ok(0) => return Err(TtyIoError::WouldBlock),
                Ok(n) => remaining = &remaining[n..],
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    /// 从输入源读取最多 `buf.len()` 字节;`Ok(0)` 表示当前无数据。
    fn read_input(&self, buf: &mut [u8]) -> TtyIoResult<usize>;

    /// 非破坏性查询输入是否可读(供 poll 快照)。
    fn poll_read(&self) -> bool {
        false
    }

    /// 后端是否仍可用(设备未移除、配对未销毁)。
    fn is_active(&self) -> bool {
        true
    }

    /// 执行控制请求;不支持时返回 `Invalid`。
    fn control(&self, _req: TtyControlRequest) -> TtyIoResult<TtyControlResponse> {
        Err(TtyIoError::Invalid)
    }

    /// 窗口大小变化通知(pts 对端同步、TTY 层发 SIGWINCH 后回调)。
    fn winsize_changed(&self, _winsize: UserWinSize) {}

    /// VT 切换 / pty 挂起时的激活通知。
    fn activate(&self, _active: bool) {}

    /// 设备移除或对端关闭时的挂起通知(行规程据此发 SIGHUP)。
    fn hangup(&self) {}
}

/// 规范模式行缓冲状态。
#[derive(Default)]
struct TtyLineState {
    line: Vec<u8>,
    ready: VecDeque<u8>,
    eof_pending: bool,
}

impl TtyLineState {
    fn clear(&mut self) {
        self.line.clear();
        self.ready.clear();
        self.eof_pending = false;
    }
}

/// 一个终端实例的共享行规程状态。
///
/// devtmpfs 可能把同一个底层终端同时投影成多个 `/dev` 节点(如 console 别名
/// 与驱动自己的串口节点);termios、窗口大小、前台进程组和规范模式行缓冲
/// 属于终端本身,必须在这些节点和所有 open fd 之间共享。每个 fd 只保留
/// 自己的状态标志。
pub struct TtyCore {
    driver: Arc<dyn TerminalDriver>,
    termios: Spinlock<UserTermios>,
    winsize: Spinlock<UserWinSize>,
    foreground_pgrp: Spinlock<i32>,
    line_state: Spinlock<TtyLineState>,
    hung_up: AtomicBool,
}

/// 异步输入泵的单次 drain 上限。
const TTY_ASYNC_PUMP_LIMIT: usize = 256;

impl TtyCore {
    pub fn new(driver: Arc<dyn TerminalDriver>) -> Self {
        Self {
            driver,
            termios: Spinlock::new(UserTermios::new_default()),
            winsize: Spinlock::new(UserWinSize::default_console()),
            foreground_pgrp: Spinlock::new(0),
            line_state: Spinlock::new(TtyLineState::default()),
            hung_up: AtomicBool::new(false),
        }
    }

    pub fn driver(&self) -> &dyn TerminalDriver {
        self.driver.as_ref()
    }

    pub fn is_active(&self) -> bool {
        self.driver.is_active()
    }

    /// 后端输入队列是否有可读数据(供 poll 快照)。
    pub fn poll_read(&self) -> bool {
        self.driver.poll_read()
    }

    /// 规范/非规范行缓冲中是否有已就绪字节。
    pub fn has_ready_input(&self) -> bool {
        let state = self.line_state.lock();
        state.eof_pending || !state.ready.is_empty()
    }

    /// 记录最近的 tty reader 进程组,供 timer 输入泵在没有 reader 调用栈时
    /// 仍能把 Ctrl-C 发给合理的前台组。
    pub fn remember_reader_pgrp(&self) {
        let Ok(pgrp) = operation::getpgid(0) else {
            return;
        };
        if pgrp <= 0 {
            return;
        }
        let mut foreground = self.foreground_pgrp.lock();
        if *foreground <= 0 {
            // 某些 shell 在当前作业控制尚不完整时不会显式 TIOCSPGRP。
            // 记录最近的 tty reader 进程组,供 timer 输入泵在没有 reader
            // 调用栈时仍能把 Ctrl-C 发给合理的前台组。
            *foreground = pgrp;
        }
    }

    pub fn current_or_stored_pgrp(&self) -> Result<i32, Errno> {
        let stored = *self.foreground_pgrp.lock();
        if stored > 0 {
            Ok(stored)
        } else {
            operation::getpgid(0)
        }
    }

    pub fn session_id(&self) -> Result<i32, Errno> {
        operation::getsid(0)
    }

    pub(crate) fn write_tty_bytes(&self, buf: &[u8], termios: UserTermios) -> TtyIoResult<()> {
        if self.hung_up.load(Ordering::Acquire) {
            return Err(TtyIoError::NoDevice);
        }
        if buf.is_empty() {
            return Ok(());
        }
        if !termios.opost_onlcr() {
            return self.driver.write_all_output(buf);
        }

        let mut cooked = Vec::with_capacity(buf.len());
        for &byte in buf {
            if byte == b'\n' {
                cooked.push(b'\r');
                cooked.push(b'\n');
            } else {
                cooked.push(byte);
            }
        }
        self.driver.write_all_output(&cooked)
    }

    fn dequeue_ready(&self, buf: &mut [u8]) -> Option<usize> {
        let mut state = self.line_state.lock();
        if state.eof_pending {
            state.eof_pending = false;
            return Some(0);
        }
        if state.ready.is_empty() {
            return None;
        }
        let mut n = 0usize;
        while n < buf.len() {
            let Some(byte) = state.ready.pop_front() else {
                break;
            };
            buf[n] = byte;
            n += 1;
        }
        Some(n)
    }

    fn dequeue_pending_bytes(&self, buf: &mut [u8]) -> usize {
        let mut state = self.line_state.lock();
        let mut n = 0usize;
        while n < buf.len() {
            let Some(byte) = state.ready.pop_front() else {
                break;
            };
            buf[n] = byte;
            n += 1;
        }
        n
    }

    pub(crate) fn send_fg_signal(&self, sig: sched::SignalNumber) {
        let stored = *self.foreground_pgrp.lock();
        let current = operation::getpgid(0).ok().filter(|pgrp| *pgrp > 0);
        let primary = if stored > 0 { Some(stored) } else { current };

        if let Some(pgrp) = primary {
            // 前台进程组是 TTY 的内部对象关系,不应通过 kill(-PGID) 的
            // 用户态 pid 编码间接表达;PGID==1 会与特殊广播形式冲突。
            let _ = operation::kill_process_group(pgrp, Some(sig));
        }

        if stored > 0 {
            if let Some(current_pgrp) = current {
                if current_pgrp != stored {
                    // 当前作业控制还不完整:某些 shell 会把 TTY 前台组留在 shell 自己,
                    // 但前台程序已经在这个读路径里消费到 VINTR/VQUIT/VSUSP。补发给当前
                    // 读者进程组,避免 Ctrl-C 只打到 shell,真正阻塞的程序继续睡眠。
                    let _ = operation::kill_process_group(current_pgrp, Some(sig));
                }
            }
        }
    }

    fn echo_signal_char(&self, sig: sched::SignalNumber, termios: UserTermios) {
        if !termios.echo() {
            return;
        }
        let bytes = if sig == sched::SignalNumber::SIGINT {
            &b"^C\n"[..]
        } else if sig == sched::SignalNumber::SIGQUIT {
            &b"^\\\n"[..]
        } else if sig == sched::SignalNumber::SIGTSTP {
            &b"^Z\n"[..]
        } else {
            &b"\n"[..]
        };
        let _ = self.write_tty_bytes(bytes, termios);
    }

    fn handle_input_signal(&self, ch: u8, termios: UserTermios) -> TtyIoResult<()> {
        let Some(sig) = termios.signal_for_input(ch) else {
            return Ok(());
        };
        self.send_fg_signal(sig);
        self.line_state.lock().clear();
        self.echo_signal_char(sig, termios);
        Err(TtyIoError::Interrupted)
    }

    fn handle_async_input_signal(&self, ch: u8, termios: UserTermios) -> TtyIoResult<()> {
        // 异步输入泵没有用户态 read() 调用栈,不能完全依赖当前 termios 的
        // ISIG 状态:BusyBox shell 在启动前台命令前可能短暂把终端切到 raw
        // 模式。此时 Ctrl-C 如果按普通字节排队,就会等到前台命令结束后才
        // 被 shell 读到。这里仅对 VINTR/VQUIT/VSUSP 做兜底信号化,普通字节
        // 仍进入行规程 pending 队列,避免破坏 raw 模式数据流。
        let sig = termios.signal_for_input(ch).or_else(|| {
            if ch == 0 {
                None
            } else if ch == termios.vintr() {
                Some(sched::SignalNumber::SIGINT)
            } else if ch == termios.vquit() {
                Some(sched::SignalNumber::SIGQUIT)
            } else if ch == termios.vsusp() {
                Some(sched::SignalNumber::SIGTSTP)
            } else {
                None
            }
        });
        let Some(sig) = sig else {
            return Ok(());
        };
        self.send_fg_signal(sig);
        self.line_state.lock().clear();
        self.echo_signal_char(sig, termios);
        Err(TtyIoError::Interrupted)
    }

    fn pump_tty_canonical_once(&self, termios: UserTermios) -> TtyIoResult<bool> {
        let mut byte = [0u8; 1];
        let n = self.driver.read_input(&mut byte)?;
        if n == 0 {
            return Ok(false);
        }

        let mut ch = byte[0];
        if termios.icrnl() && ch == b'\r' {
            ch = b'\n';
        }
        if termios.ixon() && (ch == 17 || ch == 19) {
            return Ok(true);
        }
        self.handle_input_signal(ch, termios)?;

        let mut echo_bytes: Option<Vec<u8>> = None;
        {
            let mut state = self.line_state.lock();
            if ch == termios.verase() && ch != 0 {
                if state.line.pop().is_some() && termios.echo() {
                    echo_bytes = Some(if termios.echoe() {
                        Vec::from(&b"\x08 \x08"[..])
                    } else {
                        Vec::from(&[ch][..])
                    });
                }
            } else if ch == termios.vkill() && ch != 0 {
                let erased = state.line.len();
                state.line.clear();
                if erased != 0 && termios.echo() {
                    let mut out = Vec::new();
                    if termios.echoe() {
                        out.reserve(erased * 3);
                        for _ in 0..erased {
                            out.extend_from_slice(b"\x08 \x08");
                        }
                    }
                    if termios.echok() {
                        out.push(b'\n');
                    }
                    if !out.is_empty() {
                        echo_bytes = Some(out);
                    }
                }
            } else if ch == termios.veof() && ch != 0 {
                if state.line.is_empty() {
                    state.eof_pending = true;
                } else {
                    while let Some(byte) = state.line.first().copied() {
                        state.ready.push_back(byte);
                        state.line.remove(0);
                    }
                }
            } else {
                state.line.push(ch);
                if termios.echo() {
                    echo_bytes = Some(Vec::from(&[ch][..]));
                }
                if ch == b'\n' {
                    while let Some(byte) = state.line.first().copied() {
                        state.ready.push_back(byte);
                        state.line.remove(0);
                    }
                }
            }
        }

        if let Some(bytes) = echo_bytes.as_deref() {
            let _ = self.write_tty_bytes(bytes, termios);
        }
        Ok(true)
    }

    fn process_raw_input_bytes(
        &self,
        termios: UserTermios,
        buf: &mut [u8],
        force_control_signal: bool,
    ) -> TtyIoResult<usize> {
        let mut out = 0usize;
        for idx in 0..buf.len() {
            let mut ch = buf[idx];
            if termios.icrnl() && ch == b'\r' {
                ch = b'\n';
            }
            if termios.ixon() && (ch == 17 || ch == 19) {
                continue;
            }
            if force_control_signal {
                self.handle_async_input_signal(ch, termios)?;
            } else {
                self.handle_input_signal(ch, termios)?;
            }
            buf[out] = ch;
            out += 1;
        }
        if out != 0 && termios.echo() {
            let _ = self.write_tty_bytes(&buf[..out], termios);
        }
        Ok(out)
    }

    fn pump_tty_raw_once(&self, termios: UserTermios) -> TtyIoResult<bool> {
        let mut byte = [0u8; 1];
        let n = self.driver.read_input(&mut byte)?;
        if n == 0 {
            return Ok(false);
        }

        let produced = self.process_raw_input_bytes(termios, &mut byte, true)?;
        if produced == 0 {
            return Ok(true);
        }
        let mut state = self.line_state.lock();
        state
            .ready
            .try_reserve(produced)
            .map_err(|_| TtyIoError::NoSpace)?;
        for &byte in &byte[..produced] {
            state.ready.push_back(byte);
        }
        Ok(true)
    }

    /// 从已打开的 TTY 主动拉取输入,供 timer tick 路径调用。
    ///
    /// 串口中断只能说明底层 FIFO 有字节,不能替代终端行规程。若前台程序
    /// 没有调用 `read()`(例如 `sleep`),Ctrl-C/Ctrl-\ /Ctrl-Z 仍必须被
    /// 终端识别并投递给前台进程组;因此这里在 tick 上做一次有界 drain。
    /// 非规范模式下普通字节会进入 TTY pending 队列,由之后的 read() 取走;
    /// 控制字符则立即处理,避免 raw-mode shell 启动前台程序后 Ctrl-C 滞留。
    /// 终端挂起(设备移除 / pts master 关闭)。
    ///
    /// 置挂起位、通知后端,并向前台进程组发 SIGHUP(Linux 语义);此后
    /// 读返回已缓冲数据后 EOF,写返回 `NoDevice`。
    pub fn hangup(&self) {
        self.hung_up.store(true, Ordering::Release);
        self.driver.hangup();
        self.send_fg_signal(sched::SignalNumber::SIGHUP);
    }

    pub fn is_hung_up(&self) -> bool {
        self.hung_up.load(Ordering::Acquire)
    }

    pub fn drain_tty_input(&self, termios: UserTermios) {
        for _ in 0..TTY_ASYNC_PUMP_LIMIT {
            let result = if termios.canonical() {
                self.pump_tty_canonical_once(termios)
            } else {
                self.pump_tty_raw_once(termios)
            };
            match result {
                Ok(true) | Err(TtyIoError::Interrupted) => {}
                Ok(false) | Err(_) => break,
            }
        }
    }

    pub fn read_tty_canonical(&self, buf: &mut [u8], termios: UserTermios) -> TtyIoResult<usize> {
        loop {
            if self.hung_up.load(Ordering::Acquire) {
                // 挂起后先取走已缓冲数据,再返回 EOF。
                return Ok(self.dequeue_ready(buf).unwrap_or(0));
            }
            if let Some(n) = self.dequeue_ready(buf) {
                return Ok(n);
            }
            if !self.pump_tty_canonical_once(termios)? {
                return Err(TtyIoError::WouldBlock);
            }
        }
    }

    pub fn read_tty_raw(&self, buf: &mut [u8], termios: UserTermios) -> TtyIoResult<usize> {
        if self.hung_up.load(Ordering::Acquire) {
            // 挂起后先取走已缓冲数据,再返回 EOF。
            let filled = self.dequeue_pending_bytes(buf);
            if filled != 0 {
                return Ok(filled);
            }
            return Ok(0);
        }
        let want = termios.vmin().max(1) as usize;
        let mut filled = self.dequeue_pending_bytes(buf);
        if filled >= want || filled == buf.len() {
            return Ok(filled);
        }
        loop {
            let start = filled;
            let n = self.driver.read_input(&mut buf[start..])?;
            if n != 0 {
                let produced =
                    self.process_raw_input_bytes(termios, &mut buf[start..start + n], false)?;
                filled += produced;
                if filled >= want || filled == buf.len() {
                    return Ok(filled);
                }
            } else {
                if filled != 0 && termios.vtime() == 0 {
                    return Ok(filled);
                }
                return Err(TtyIoError::WouldBlock);
            }
        }
    }
}

impl crate::vfs::user_api::tty::TtyIoctlState for TtyCore {
    fn termios(&self) -> UserTermios {
        *self.termios.lock()
    }

    fn set_termios(&self, termios: UserTermios) {
        *self.termios.lock() = termios;
    }

    fn winsize(&self) -> UserWinSize {
        *self.winsize.lock()
    }

    fn set_winsize(&self, winsize: UserWinSize) {
        *self.winsize.lock() = winsize;
        // Linux: TIOCSWINSZ 成功后向前台进程组发 SIGWINCH。
        self.driver.winsize_changed(winsize);
        self.send_fg_signal(sched::SignalNumber::SIGWINCH);
    }

    fn clear_line_state(&self) {
        self.line_state.lock().clear();
    }

    fn foreground_pgrp(&self) -> i32 {
        *self.foreground_pgrp.lock()
    }

    fn set_foreground_pgrp(&self, pgrp: i32) {
        *self.foreground_pgrp.lock() = pgrp;
    }

    fn control(&self, req: TtyControlRequest) -> Result<TtyControlResponse, Errno> {
        self.driver
            .control(req)
            .map_err(|err| match err {
                TtyIoError::WouldBlock | TtyIoError::TimedOut => Errno::EAGAIN,
                TtyIoError::Interrupted => Errno::EINTR,
                TtyIoError::NoSpace => Errno::ENOMEM,
                TtyIoError::Io => Errno::EIO,
                TtyIoError::NoDevice => Errno::ENODEV,
                TtyIoError::Unsupported => Errno::ENOTTY,
                TtyIoError::Busy => Errno::EBUSY,
                TtyIoError::Invalid => Errno::EINVAL,
            })
    }
}

// ── CharDevice 后端适配 ───────────────────────────────────────────────────────

/// 把现有 [`CharDevice`] 适配为 [`TerminalDriver`]。
///
/// 串口、console 等既有字符设备通过本适配器接入 TTY 核心层,行为与迁移前
/// 完全一致(阻塞写走设备级 `write_all`,控制请求映射到 `CharControlRequest`)。
pub struct CharDeviceTerminalDriver {
    dev: CharDevice,
}

impl CharDeviceTerminalDriver {
    pub fn new(dev: CharDevice) -> Self {
        Self { dev }
    }

    pub fn dev(&self) -> &CharDevice {
        &self.dev
    }
}

impl TerminalDriver for CharDeviceTerminalDriver {
    fn write_output(&self, buf: &[u8]) -> TtyIoResult<usize> {
        self.dev.write(buf).map_err(map_char_err)
    }

    fn write_all_output(&self, buf: &[u8]) -> TtyIoResult<()> {
        self.dev.write_all(buf).map_err(map_char_err)
    }

    fn read_input(&self, buf: &mut [u8]) -> TtyIoResult<usize> {
        self.dev.read(buf).map_err(map_char_err)
    }

    fn poll_read(&self) -> bool {
        self.dev.poll_read()
    }

    fn is_active(&self) -> bool {
        self.dev.is_active()
    }

    fn winsize_changed(&self, winsize: UserWinSize) {
        self.dev.winsize_changed(winsize);
    }

    fn control(&self, req: TtyControlRequest) -> TtyIoResult<TtyControlResponse> {
        let request = match req {
            TtyControlRequest::DrainTx => CharControlRequest::DrainTx,
            TtyControlRequest::FlushTx => CharControlRequest::FlushTx,
            TtyControlRequest::FlushRx => CharControlRequest::FlushRx,
            TtyControlRequest::FlushBoth => CharControlRequest::FlushBoth,
            TtyControlRequest::SetSerialConfig { baud } => {
                CharControlRequest::SetSerialConfig { baud }
            }
            TtyControlRequest::SendBreak { duration_ms } => {
                CharControlRequest::SendBreak { duration_ms }
            }
            TtyControlRequest::GetInputQueueLen => CharControlRequest::GetInputQueueLen,
            TtyControlRequest::GetOutputQueueLen => CharControlRequest::GetOutputQueueLen,
        };
        match self.dev.control(request) {
            Ok(CharControlResponse::Done) => Ok(TtyControlResponse::Done),
            Ok(CharControlResponse::U32(value)) => Ok(TtyControlResponse::U32(value)),
            Err(ControlError::Unsupported) => Err(TtyIoError::Unsupported),
            Err(ControlError::Invalid) => Err(TtyIoError::Invalid),
            Err(ControlError::NoDevice) => Err(TtyIoError::NoDevice),
            Err(ControlError::Busy) => Err(TtyIoError::Busy),
            Err(ControlError::Permission) => Err(TtyIoError::Invalid),
            Err(ControlError::Io) => Err(TtyIoError::Io),
        }
    }
}

fn map_char_err(e: CharIoError) -> TtyIoError {
    match e {
        CharIoError::HardwareError => TtyIoError::Io,
        CharIoError::Unavailable => TtyIoError::NoDevice,
        CharIoError::Interrupted => TtyIoError::Interrupted,
        CharIoError::Timeout => TtyIoError::TimedOut,
    }
}

// ── 按底层设备共享的行规程实例 ───────────────────────────────────────────────

static TTY_CORES: Spinlock<BTreeMap<String, Weak<TtyCore>>> = Spinlock::new(BTreeMap::new());

fn fallible_string(value: &str) -> Option<String> {
    let mut out = String::new();
    out.try_reserve(value.len()).ok()?;
    out.push_str(value);
    Some(out)
}

/// 返回一个底层字符设备的共享行规程实例。
///
/// 同一个底层 TTY 可能被投影成多个 `/dev` 节点,例如稳定的 console 别名
/// 和驱动自己的串口节点。行规程状态必须按设备共享,不能按 open fd 分裂。
/// 共享状态缓存只是优化;如果名称键分配失败,调用方仍可持有独立状态继续
/// 工作,不能因为缓存失败阻断字符设备打开路径。
pub fn shared_tty_core(dev: &CharDevice) -> Option<Arc<TtyCore>> {
    if !dev.is_tty() {
        return None;
    }

    let mut cores = TTY_CORES.lock();
    if let Some(core) = cores.get(dev.fw_name()).and_then(Weak::upgrade) {
        return Some(core);
    }

    let driver: Arc<dyn TerminalDriver> = Arc::new(CharDeviceTerminalDriver::new(dev.clone()));
    let core = Arc::new(TtyCore::new(driver));
    if let Some(key) = fallible_string(dev.fw_name()) {
        cores.insert(key, Arc::downgrade(&core));
    }
    Some(core)
}

/// 当前存活的共享行规程实例快照(供 tick 输入泵使用)。
pub fn active_tty_cores() -> Vec<Arc<TtyCore>> {
    let mut out = Vec::new();
    {
        let mut cores = TTY_CORES.lock();
        cores.retain(|_, weak| {
            let Some(core) = weak.upgrade() else {
                return false;
            };
            out.push(core);
            true
        });
    }
    out
}

/// 按节点名(fw_name)查询共享行规程实例。
///
/// 供 VT 泵等需要按名定位具体终端核心的路径使用;未创建或已失效时返回 None。
pub fn lookup_tty_core(name: &str) -> Option<Arc<TtyCore>> {
    TTY_CORES.lock().get(name).and_then(Weak::upgrade)
}
