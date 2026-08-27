//! 伪终端(pty)与 devpts 支持。
//!
//! 与 Linux 的 `drivers/tty/pty.c` 对应但保持投影式设计:
//! - master 不是 tty:`/dev/ptmx`(5:2)open 时分配一对 [`PtyPair`],
//!   返回原始字节读写 + pty ioctl 的 `PtyMasterFileOps`;
//! - slave 是完整 tty:`/dev/pts/N`(136:N)由 devpts 动态投影,复用
//!   [`TtyCore`](crate::dev::tty::core::TtyCore) 的全部行规程/作业控制;
//! - 双向有界环形缓冲承载数据流,满时经 poll 阻塞(不忙等);
//! - master 全部关闭 → slave 挂起(SIGHUP + EOF/EIO),与 Linux 一致。

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use errno::Errno;
use vfs::file::{FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::sync::Spinlock;

use crate::dev::char::{CharDevice, CharIoError};
use crate::dev::control::{CharControlRequest, CharControlResponse, ControlError};
use crate::dev::tty::core::{
    TerminalDriver, TtyControlRequest, TtyControlResponse, TtyCore, TtyIoError, TtyIoResult,
    lookup_tty_core,
};
use crate::vfs::user_api::ioctl::{
    read_bytes_from_user, read_i32_from_user, write_bytes_to_user, write_i32_to_user,
    write_u32_to_user,
};
use crate::vfs::user_api::tty::{TtyIoctlState, UserWinSize};

/// pty 对数上限(与 Linux 默认同量级)。
pub const PTY_MAX: u32 = 256;

/// 单向环形缓冲容量(与 Linux n_tty 输出缓冲同量级)。
const PTY_RING_CAP: usize = 4096;

// ── pty ioctl 号(与 Linux 一致) ─────────────────────────────────────────────

pub const TIOCGPTN: usize = 0x8004_5430;
pub const TIOCSPTLCK: usize = 0x4004_5431;
pub const TIOCGPTLCK: usize = 0x8004_5439;
pub const TIOCGPTPEER: usize = 0x5441;
pub const TIOCSIG: usize = 0x4004_5436;
const TIOCPKT: usize = 0x5420;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;

/// 有界字节环。
struct Ring {
    buf: VecDeque<u8>,
    cap: usize,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> usize {
        let mut n = 0usize;
        for &byte in bytes {
            if self.buf.len() >= self.cap {
                break;
            }
            self.buf.push_back(byte);
            n += 1;
        }
        n
    }

    fn pop(&mut self, out: &mut [u8]) -> usize {
        let mut n = 0usize;
        while n < out.len() {
            let Some(byte) = self.buf.pop_front() else {
                break;
            };
            out[n] = byte;
            n += 1;
        }
        n
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn has_space(&self) -> bool {
        self.buf.len() < self.cap
    }

    fn clear(&mut self) {
        self.buf.clear();
    }
}

/// 一对伪终端。
pub struct PtyPair {
    index: u32,
    /// master → slave(由 slave 行规程消费)。
    in_ring: Spinlock<Ring>,
    /// slave → master(由 master 读取)。
    out_ring: Spinlock<Ring>,
    winsize: Spinlock<UserWinSize>,
    /// TIOCSPTLCK 锁定时禁止打开 slave。
    locked: AtomicBool,
    /// TIOCPKT 包模式。
    packet_mode: AtomicBool,
    master_open: AtomicU32,
    slave_open: AtomicU32,
    /// master 全部关闭后置位(slave 挂起)。
    slave_hung_up: AtomicBool,
    /// 是否已从管理器注销(避免重复释放)。
    destroyed: AtomicBool,
}

impl PtyPair {
    fn new(index: u32) -> Self {
        Self {
            index,
            in_ring: Spinlock::new(Ring::new(PTY_RING_CAP)),
            out_ring: Spinlock::new(Ring::new(PTY_RING_CAP)),
            winsize: Spinlock::new(UserWinSize::default_console()),
            locked: AtomicBool::new(false),
            packet_mode: AtomicBool::new(false),
            master_open: AtomicU32::new(0),
            slave_open: AtomicU32::new(0),
            slave_hung_up: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
        }
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    /// slave 节点名(`pts/N`),同时是共享行规程的 fw_name 键。
    pub fn name(&self) -> String {
        let mut out = String::new();
        out.try_reserve(12).ok();
        out.push_str("pts/");
        out.push_str(&self.index.to_string());
        out
    }

    fn slave_core(&self) -> Option<Arc<TtyCore>> {
        lookup_tty_core(&self.name())
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Acquire)
    }

    fn note_master_open(&self, delta: i32) {
        let count = self.master_open.load(Ordering::Acquire) as i64;
        self.master_open.store(
            (count + i64::from(delta)).clamp(0, i64::from(u32::MAX)) as u32,
            Ordering::Release,
        );
        self.check_destroy();
    }

    fn note_slave_open(&self, delta: i32) {
        let count = self.slave_open.load(Ordering::Acquire) as i64;
        let next = (count + i64::from(delta)).clamp(0, i64::from(u32::MAX)) as u32;
        self.slave_open.store(next, Ordering::Release);
        if next == 0 && count > 0 {
            // 最后一个 slave fd 关闭:master 读侧进入 EIO。
            self.out_ring.lock().clear();
        }
        self.check_destroy();
    }

    /// master 全部关闭 → slave 挂起。
    fn note_master_closed_all(&self) {
        self.slave_hung_up.store(true, Ordering::Release);
        if let Some(core) = self.slave_core() {
            core.hangup();
        }
    }

    /// 两端都关闭后从管理器注销并释放编号。
    fn check_destroy(&self) {
        if self.destroyed.load(Ordering::Acquire) {
            return;
        }
        if self.master_open.load(Ordering::Acquire) == 0
            && self.slave_open.load(Ordering::Acquire) == 0
            && self.destroyed.swap(true, Ordering::AcqRel) == false
        {
            pty_manager().destroy(self.index);
        }
    }

    /// slave 输出 → master 读队列(带包模式前缀)。
    fn push_slave_output(&self, bytes: &[u8]) -> usize {
        let mut ring = self.out_ring.lock();
        let mut n = 0usize;
        if self.packet_mode.load(Ordering::Acquire) {
            if ring.buf.is_empty() && !bytes.is_empty() {
                // 包模式:数据前加 0x00 控制字节(Linux TIOCPKT 语义)。
                ring.buf.push_back(0);
                n += 1;
            }
        }
        n += ring.push(bytes);
        n
    }
}

/// master 文件操作(非 tty:原始字节 + pty ioctl)。
pub struct PtyMasterFileOps {
    pair: Arc<PtyPair>,
    nonblock: AtomicBool,
}

impl PtyMasterFileOps {
    fn new(pair: Arc<PtyPair>, nonblock: bool) -> Self {
        pair.note_master_open(1);
        Self {
            pair,
            nonblock: AtomicBool::new(nonblock),
        }
    }

    pub fn pair(&self) -> &Arc<PtyPair> {
        &self.pair
    }

    /// 打开 slave 并返回其 FileOps(供 TIOCGPTPEER 与 devpts 节点复用)。
    pub fn open_slave_ops(
        &self,
        nonblock: bool,
    ) -> crate::vfs::error::VfsResult<Box<dyn FileOps + Send + Sync>> {
        let dev = self.pair.slave_char_device()?;
        crate::vfs::devtmpfs::char_dev_file_ops(dev, nonblock)
    }

    fn send_signal(&self, sig: i32) -> Result<(), Errno> {
        let Some(sig) = sched::SignalNumber::from_raw(sig) else {
            return Err(Errno::EINVAL);
        };
        if let Some(core) = self.pair.slave_core() {
            core.send_fg_signal(sig);
        }
        Ok(())
    }
}

impl FileOps for PtyMasterFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> crate::vfs::error::VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut ring = self.pair.out_ring.lock();
        let n = ring.pop(buf);
        if n != 0 {
            return Ok(n);
        }
        // slave 全部关闭且无数据 → EIO(Linux 语义)。
        if self.pair.slave_open.load(Ordering::Acquire) == 0 {
            return Err(crate::vfs::error::VfsError::Io);
        }
        Err(crate::vfs::error::VfsError::WouldBlock)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> crate::vfs::error::VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pair.slave_hung_up.load(Ordering::Acquire) {
            return Err(crate::vfs::error::VfsError::Io);
        }
        let mut ring = self.pair.in_ring.lock();
        let n = ring.push(buf);
        if n == 0 {
            return Err(crate::vfs::error::VfsError::WouldBlock);
        }
        // 唤醒/推进 slave 行规程:输入已入队,由 tick 泵 drain。
        Ok(n)
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(vfs::file::DirEntry) -> core::ops::ControlFlow<()>,
    ) -> crate::vfs::error::VfsResult<u64> {
        Err(crate::vfs::error::VfsError::NotADirectory)
    }

    fn sync(&self) -> crate::vfs::error::VfsResult<()> {
        Ok(())
    }

    fn poll(&self, _interest: PollEvents) -> PollEvents {
        let mut events = PollEvents(0);
        let out_len = self.pair.out_ring.lock().len();
        if out_len != 0 {
            events = events.with(PollEvents::POLLIN);
        }
        if self.pair.slave_open.load(Ordering::Acquire) == 0 {
            events = events.with(PollEvents::POLLHUP);
        }
        if self.pair.in_ring.lock().has_space() {
            events = events.with(PollEvents::POLLOUT);
        }
        events
    }

    fn is_epollable(&self) -> bool {
        true
    }

    fn set_status_flags(&self, flags: OpenOptions) {
        self.nonblock.store(flags.nonblock, Ordering::Release);
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        match cmd.raw() {
            TIOCGPTN => {
                write_u32_to_user(arg, self.pair.index)?;
                Ok(0)
            }
            TIOCSPTLCK => {
                let lock = read_i32_from_user(arg)?;
                self.pair.locked.store(lock != 0, Ordering::Release);
                Ok(0)
            }
            TIOCGPTLCK => {
                write_i32_to_user(arg, i32::from(self.pair.is_locked()))?;
                Ok(0)
            }
            TIOCSIG => {
                let sig = read_i32_from_user(arg)?;
                self.send_signal(sig)?;
                Ok(0)
            }
            TIOCPKT => {
                let mode = read_i32_from_user(arg)?;
                self.pair.packet_mode.store(mode != 0, Ordering::Release);
                Ok(0)
            }
            TIOCGWINSZ => {
                let winsize = *self.pair.winsize.lock();
                write_bytes_to_user(arg, &winsize.to_bytes())?;
                Ok(0)
            }
            TIOCSWINSZ => {
                let mut raw = [0u8; 8]; // struct winsize { u16 x4 }
                read_bytes_from_user(arg, &mut raw)?;
                let winsize = UserWinSize::from_bytes(raw);
                *self.pair.winsize.lock() = winsize;
                if let Some(core) = self.pair.slave_core() {
                    core.set_winsize(winsize);
                }
                Ok(0)
            }
            // TIOCGPTPEER 需要分配新 fd,由 syscall 层经
            // [`PtyMasterFileOps::open_slave_ops`] 完成。
            TIOCGPTPEER => Err(Errno::ENOTTY),
            _ => Err(Errno::ENOTTY),
        }
    }

    fn release(&self) {
        self.pair.note_master_open(-1);
        if self.pair.master_open.load(Ordering::Acquire) == 0 {
            self.pair.note_master_closed_all();
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── slave 后端 ───────────────────────────────────────────────────────────────

/// slave 的 [`TerminalDriver`]:输出进 master 读队列,输入来自 master 写队列。
pub struct PtySlaveTerminalDriver {
    pair: Arc<PtyPair>,
}

impl TerminalDriver for PtySlaveTerminalDriver {
    fn write_output(&self, buf: &[u8]) -> TtyIoResult<usize> {
        if self.pair.slave_hung_up.load(Ordering::Acquire) {
            return Err(TtyIoError::NoDevice);
        }
        Ok(self.pair.push_slave_output(buf))
    }

    fn read_input(&self, buf: &mut [u8]) -> TtyIoResult<usize> {
        let mut ring = self.pair.in_ring.lock();
        Ok(ring.pop(buf))
    }

    fn poll_read(&self) -> bool {
        !self.pair.in_ring.lock().is_empty()
    }

    fn is_active(&self) -> bool {
        !self.pair.slave_hung_up.load(Ordering::Acquire)
    }

    fn winsize_changed(&self, winsize: UserWinSize) {
        *self.pair.winsize.lock() = winsize;
    }

    fn control(&self, req: TtyControlRequest) -> TtyIoResult<TtyControlResponse> {
        match req {
            TtyControlRequest::DrainTx | TtyControlRequest::FlushTx => Ok(TtyControlResponse::Done),
            TtyControlRequest::FlushRx | TtyControlRequest::FlushBoth => {
                self.pair.in_ring.lock().clear();
                Ok(TtyControlResponse::Done)
            }
            TtyControlRequest::GetInputQueueLen => Ok(TtyControlResponse::U32(
                self.pair.in_ring.lock().len() as u32,
            )),
            TtyControlRequest::GetOutputQueueLen => Ok(TtyControlResponse::U32(
                self.pair.out_ring.lock().len() as u32,
            )),
            TtyControlRequest::SetSerialConfig { .. } | TtyControlRequest::SendBreak { .. } => {
                Err(TtyIoError::Unsupported)
            }
        }
    }

    fn activate(&self, _active: bool) {}

    fn hangup(&self) {}
}

/// slave 的 [`CharDriver`]:通过 devtmpfs/devpts 字符节点接入共享行规程。
pub struct PtySlaveCharDriver {
    pair: Arc<PtyPair>,
}

impl PtySlaveCharDriver {
    pub fn pair(&self) -> &Arc<PtyPair> {
        &self.pair
    }
}

impl crate::dev::char::CharDriver for PtySlaveCharDriver {
    fn write(&self, buf: &[u8]) -> Result<usize, CharIoError> {
        if self.pair.slave_hung_up.load(Ordering::Acquire) {
            return Err(CharIoError::Unavailable);
        }
        Ok(self.pair.push_slave_output(buf))
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        let mut ring = self.pair.in_ring.lock();
        Ok(ring.pop(buf))
    }

    fn flush(&self) -> Result<(), CharIoError> {
        Ok(())
    }

    fn poll_read(&self) -> bool {
        !self.pair.in_ring.lock().is_empty()
    }

    fn is_tty(&self) -> bool {
        true
    }

    fn winsize_changed(&self, winsize: UserWinSize) {
        *self.pair.winsize.lock() = winsize;
    }

    fn control(&self, req: CharControlRequest) -> Result<CharControlResponse, ControlError> {
        match req {
            CharControlRequest::DrainTx | CharControlRequest::FlushTx => {
                Ok(CharControlResponse::Done)
            }
            CharControlRequest::FlushRx | CharControlRequest::FlushBoth => {
                self.pair.in_ring.lock().clear();
                Ok(CharControlResponse::Done)
            }
            CharControlRequest::GetInputQueueLen => Ok(CharControlResponse::U32(
                self.pair.in_ring.lock().len() as u32,
            )),
            CharControlRequest::GetOutputQueueLen => Ok(CharControlResponse::U32(
                self.pair.out_ring.lock().len() as u32,
            )),
            _ => Err(ControlError::Unsupported),
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ── 管理器 ───────────────────────────────────────────────────────────────────

/// pty 管理器(单例):分配/释放编号与配对,维护 devpts 节点投影。
pub struct PtyManager {
    slots: Spinlock<Vec<Option<Arc<PtyPair>>>>,
}

static MANAGER: Spinlock<Option<&'static PtyManager>> = Spinlock::new(None);

fn pty_manager() -> &'static PtyManager {
    let mut slot = MANAGER.lock();
    if let Some(existing) = *slot {
        return existing;
    }
    let leaked: &'static PtyManager = Box::leak(Box::new(PtyManager {
        slots: Spinlock::new(Vec::new()),
    }));
    *slot = Some(leaked);
    leaked
}

impl PtyManager {
    /// 分配一对 pty(从空闲槽或新编号),并在已挂载的 devpts 中创建 slave 节点。
    pub fn open() -> Result<Arc<PtyPair>, Errno> {
        let manager = pty_manager();
        let mut slots = manager.slots.lock();
        let free = slots.iter().position(Option::is_none);
        let index = match free {
            Some(pos) => pos as u32,
            None => {
                if slots.len() >= PTY_MAX as usize {
                    return Err(Errno::EAGAIN);
                }
                slots.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
                let pos = slots.len() as u32;
                slots.push(None);
                pos
            }
        };
        let pair = Arc::new(PtyPair::new(index));
        slots[index as usize] = Some(Arc::clone(&pair));
        drop(slots);
        crate::vfs::user_api::device_numbers::register_pty(index as u32);
        crate::vfs::devpts::publish_pty_slave(&pair);
        Ok(pair)
    }

    /// 按编号查询存活 pty 对。
    pub fn pair(&self, index: u32) -> Option<Arc<PtyPair>> {
        self.slots
            .lock()
            .get(index as usize)
            .and_then(Option::clone)
    }

    /// 当前全部存活 pty 对(devpts 挂载补建节点用)。
    pub fn live_pairs(&self) -> Vec<Arc<PtyPair>> {
        self.slots.lock().iter().flatten().cloned().collect()
    }

    fn destroy(&self, index: u32) {
        crate::vfs::user_api::device_numbers::unregister_pty(index);
        crate::vfs::devpts::unpublish_pty_slave(index);
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get_mut(index as usize) {
            *slot = None;
        }
    }
}

/// 为 TIOCGPTPEER 构造 slave 的 `File`(syscall 层调用)。
pub fn open_slave_file(
    pair: &Arc<PtyPair>,
    opts: vfs::file::OpenOptions,
    cred: Arc<vfs::cred::Credentials>,
) -> crate::vfs::error::VfsResult<Arc<vfs::file::File>> {
    crate::vfs::devpts::open_slave_file(pair, opts, cred)
}

/// 打开 `/dev/ptmx`:分配 pty 对并返回 master FileOps。
pub fn open_ptmx(nonblock: bool) -> Result<Box<dyn FileOps + Send + Sync>, Errno> {
    let pair = PtyManager::open()?;
    Ok(Box::new(PtyMasterFileOps::new(pair, nonblock)))
}

/// 按编号查询存活 pty 对(devpts 节点 open 用)。
pub fn lookup_pair(index: u32) -> Option<Arc<PtyPair>> {
    pty_manager().pair(index)
}

/// 当前全部存活 pty 对。
pub fn live_pairs() -> Vec<Arc<PtyPair>> {
    pty_manager().live_pairs()
}

/// 记录 slave 字符设备打开/关闭(由 devtmpfs FileOps 构造/释放时调用)。
pub fn note_pty_opened(dev: &CharDevice, delta: i32) {
    let Some(driver) = dev.downcast_driver::<PtySlaveCharDriver>() else {
        return;
    };
    driver.pair().note_slave_open(delta);
}

impl PtyPair {
    /// slave 的字符设备(每次调用构造新句柄,行规程按 fw_name 共享)。
    pub fn slave_char_device(self: &Arc<Self>) -> crate::vfs::error::VfsResult<CharDevice> {
        let driver: Arc<dyn crate::dev::char::CharDriver> = Arc::new(PtySlaveCharDriver {
            pair: Arc::clone(self),
        });
        Ok(CharDevice::from_arc(self.name().into_boxed_str(), driver))
    }
}
