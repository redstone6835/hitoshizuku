//! devtmpfs — 设备临时文件系统
//!
//! # 设计要点
//!
//! 设备节点的 inode 直接持有设备对象引用（`CharDev` 或 `Arc<BlockDev>`），
//! 而非设备名称字符串。`open()` 时零查找：已在绑定时解析，运行时直接调用。
//!
//! ```text
//! bind_char("uart0", dev: CharDev)
//!   └─ 创建 Inode，InodeOps = DevCharOps { dev }
//!         └─ open() → CharDevAdapter::new(dev)   // 无查找，直接构造
//!
//! bind_block("vda", dev: Arc<BlockDev>)
//!   └─ 创建 Inode，InodeOps = DevBlockOps { dev: Arc<BlockDev> }
//!         └─ open() → BlockDevAdapter::new(dev)  // 无查找，直接构造
//! ```
//!
//! # 文件系统结构
//!
//! devtmpfs 只有一层目录（根目录下直接挂设备节点），根目录 inode 维护
//! `name → Arc<Inode>` 的 `BTreeMap`，作为 `lookup` 和 `readdir` 的数据源。
//!
//! 整个文件系统通过 `mount -t devtmpfs` 挂载到 `/dev`，之后通过
//! [`DevTmpfsSuperblockOps::bind_char`] / [`DevTmpfsSuperblockOps::bind_block`]
//! 动态增删节点。

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use errno::Errno;
use sched::operation as sched_operation;
use vfs::cred::{Credentials, Gid, Uid};
use vfs::dentry::{Dentry, SmallStr};
use vfs::error::{VfsError, VfsResult};
use vfs::file::{DirEntry, FileOps, IoctlCmd, OpenOptions, PollEvents};
use vfs::inode::{Inode, InodeId, InodeMeta, InodeOps};
use vfs::mount::MountFlags;
use vfs::stat::{DevId, FileMode, FileType, FsId, FsStat, Timespec};
use vfs::superblock::{FsDriver, FsDriverFlags, Superblock, SuperblockOps};
use vfs::sync::Spinlock;

use crate::dev::block::{
    BlockCompletion, BlockDevice, BlockFeatures, BlockIoCompletion, BlockIoError, BlockIoRequest,
    BlockRange, BlockSubmitError,
};
use crate::dev::char::{CharDevice, CharDeviceKind, CharIoError};
use crate::mm::{copy_from_user, copy_to_user};

// ───────── 全局实例计数器 ─────────

static DEVTMPFS_INSTANCE_COUNTER: AtomicU64 = AtomicU64::new(1);

// ───────── 字符设备 FileOps（内联适配器） ─────────

fn map_char_err(e: CharIoError) -> VfsError {
    match e {
        CharIoError::HardwareError => VfsError::Io,
        CharIoError::Unavailable => VfsError::NoDevice,
        CharIoError::Timeout => VfsError::TimedOut,
    }
}

fn map_char_errno(e: CharIoError) -> Errno {
    map_char_err(e).to_errno()
}

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TCSETSW: usize = 0x5403;
const TCSETSF: usize = 0x5404;
const TCSBRK: usize = 0x5409;
const TCXONC: usize = 0x540a;
const TCFLSH: usize = 0x540b;
const TIOCEXCL: usize = 0x540c;
const TIOCNXCL: usize = 0x540d;
const TIOCSCTTY: usize = 0x540e;
const TIOCGPGRP: usize = 0x540f;
const TIOCSPGRP: usize = 0x5410;
const TIOCOUTQ: usize = 0x5411;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const FIONREAD: usize = 0x541b;
const FIONBIO: usize = 0x5421;
const TIOCNOTTY: usize = 0x5422;
const TIOCSETD: usize = 0x5423;
const TIOCGETD: usize = 0x5424;
const TCSBRKP: usize = 0x5425;
const TIOCGSID: usize = 0x5429;
const TCGETS2: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, b'T' as usize, 0x2a, LINUX_TERMIOS2_LEN).raw();
const TCSETS2: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'T' as usize, 0x2b, LINUX_TERMIOS2_LEN).raw();
const TCSETSW2: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'T' as usize, 0x2c, LINUX_TERMIOS2_LEN).raw();
const TCSETSF2: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_WRITE, b'T' as usize, 0x2d, LINUX_TERMIOS2_LEN).raw();

const BLKROGET: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 94, 0).raw();
const BLKGETSIZE: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 96, 0).raw();
const BLKFLSBUF: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 97, 0).raw();
const BLKSSZGET: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 104, 0).raw();
const BLKBSZGET: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, 0x12, 112, core::mem::size_of::<usize>()).raw();
const BLKGETSIZE64: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, 0x12, 114, core::mem::size_of::<usize>()).raw();
const BLKIOMIN: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 120, 0).raw();
const BLKIOOPT: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 121, 0).raw();
const BLKALIGNOFF: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 122, 0).raw();
const BLKPBSZGET: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 123, 0).raw();
const BLKDISCARDZEROES: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 124, 0).raw();
const BLKROTATIONAL: usize = IoctlCmd::from_parts(IoctlCmd::IOC_NONE, 0x12, 126, 0).raw();
const BLKGETDISKSEQ: usize =
    IoctlCmd::from_parts(IoctlCmd::IOC_READ, 0x12, 128, core::mem::size_of::<u64>()).raw();

const LINUX_TERMIOS_LEN: usize = 36;
const LINUX_TERMIOS2_LEN: usize = 44;
const LINUX_WINSIZE_LEN: usize = 8;

#[derive(Clone, Copy)]
struct LinuxTermios {
    raw: [u8; LINUX_TERMIOS_LEN],
}

impl LinuxTermios {
    fn new_default() -> Self {
        let mut raw = [0u8; LINUX_TERMIOS_LEN];
        put_u32(&mut raw, 0, 0x0500); // ICRNL | IXON
        put_u32(&mut raw, 4, 0x0005); // OPOST | ONLCR
        put_u32(&mut raw, 8, 0x04bf); // B38400 | CS8 | CREAD | HUPCL
        put_u32(&mut raw, 12, 0x803b); // ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN
        raw[17] = 3; // VINTR
        raw[18] = 28; // VQUIT
        raw[19] = 127; // VERASE
        raw[20] = 21; // VKILL
        raw[21] = 4; // VEOF
        raw[22] = 0; // VTIME
        raw[23] = 1; // VMIN
        raw[25] = 17; // VSTART
        raw[26] = 19; // VSTOP
        raw[27] = 26; // VSUSP
        Self { raw }
    }

    fn as_termios2_bytes(&self) -> [u8; LINUX_TERMIOS2_LEN] {
        let mut out = [0u8; LINUX_TERMIOS2_LEN];
        out[..LINUX_TERMIOS_LEN].copy_from_slice(&self.raw);
        put_u32(&mut out, 36, 38400);
        put_u32(&mut out, 40, 38400);
        out
    }
}

#[derive(Clone, Copy)]
struct LinuxWinSize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

impl LinuxWinSize {
    const fn default_console() -> Self {
        Self {
            rows: 25,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        }
    }

    fn from_bytes(raw: [u8; LINUX_WINSIZE_LEN]) -> Self {
        Self {
            rows: u16::from_le_bytes([raw[0], raw[1]]),
            cols: u16::from_le_bytes([raw[2], raw[3]]),
            xpixel: u16::from_le_bytes([raw[4], raw[5]]),
            ypixel: u16::from_le_bytes([raw[6], raw[7]]),
        }
    }

    fn to_bytes(self) -> [u8; LINUX_WINSIZE_LEN] {
        let mut out = [0u8; LINUX_WINSIZE_LEN];
        out[0..2].copy_from_slice(&self.rows.to_le_bytes());
        out[2..4].copy_from_slice(&self.cols.to_le_bytes());
        out[4..6].copy_from_slice(&self.xpixel.to_le_bytes());
        out[6..8].copy_from_slice(&self.ypixel.to_le_bytes());
        out
    }
}

fn read_bytes_from_user(user: usize, dst: &mut [u8]) -> Result<(), Errno> {
    copy_from_user(user, dst).map_err(|e| e.as_errno())
}

fn write_bytes_to_user(user: usize, src: &[u8]) -> Result<(), Errno> {
    copy_to_user(user, src).map_err(|e| e.as_errno())
}

fn read_i32_from_user(user: usize) -> Result<i32, Errno> {
    let mut raw = [0u8; 4];
    read_bytes_from_user(user, &mut raw)?;
    Ok(i32::from_le_bytes(raw))
}

fn write_i32_to_user(user: usize, value: i32) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

fn write_u32_to_user(user: usize, value: u32) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

fn write_u64_to_user(user: usize, value: u64) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

fn write_usize_to_user(user: usize, value: usize) -> Result<(), Errno> {
    write_bytes_to_user(user, &value.to_le_bytes())
}

fn put_u32(out: &mut [u8], off: usize, value: u32) {
    out[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

struct CharDevFileOps {
    dev: CharDevice,
    nonblock: AtomicBool,
    termios: Spinlock<LinuxTermios>,
    winsize: Spinlock<LinuxWinSize>,
    foreground_pgrp: Spinlock<i32>,
}

impl CharDevFileOps {
    fn new(dev: CharDevice, nonblock: bool) -> Self {
        Self {
            dev,
            nonblock: AtomicBool::new(nonblock),
            termios: Spinlock::new(LinuxTermios::new_default()),
            winsize: Spinlock::new(LinuxWinSize::default_console()),
            foreground_pgrp: Spinlock::new(0),
        }
    }

    fn is_tty(&self) -> bool {
        matches!(
            self.dev.kind(),
            CharDeviceKind::StandardSerial
                | CharDeviceKind::Ns16550
                | CharDeviceKind::VirtualTerminal
                | CharDeviceKind::Console
        )
    }

    fn current_or_stored_pgrp(&self) -> Result<i32, Errno> {
        let stored = *self.foreground_pgrp.lock();
        if stored > 0 {
            Ok(stored)
        } else {
            sched_operation::getpgid(0)
        }
    }
}

impl FileOps for CharDevFileOps {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.is_empty() || self.nonblock.load(Ordering::Acquire) || !self.is_tty() {
            return self.dev.read(buf).map_err(map_char_err);
        }

        loop {
            let n = self.dev.read(buf).map_err(map_char_err)?;
            if n != 0 {
                return Ok(n);
            }
            let _ = sched_operation::sched_yield();
            core::hint::spin_loop();
        }
    }
    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        self.dev.write(buf).map_err(map_char_err)
    }
    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }
    fn sync(&self) -> VfsResult<()> {
        self.dev.flush().map_err(map_char_err)
    }
    fn poll(&self, _interest: PollEvents) -> PollEvents {
        if !self.dev.is_active() {
            return PollEvents::POLLERR.with(PollEvents::POLLHUP);
        }
        PollEvents::POLLIN.with(PollEvents::POLLOUT)
    }
    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        if !self.dev.is_active() {
            return Err(Errno::ENODEV);
        }
        if !self.is_tty() {
            return Err(Errno::ENOTTY);
        }

        match cmd.raw() {
            TCGETS => {
                let termios = *self.termios.lock();
                write_bytes_to_user(arg, &termios.raw)?;
                Ok(0)
            }
            TCGETS2 => {
                let termios = *self.termios.lock();
                write_bytes_to_user(arg, &termios.as_termios2_bytes())?;
                Ok(0)
            }
            TCSETS | TCSETSW | TCSETSF => {
                let mut raw = [0u8; LINUX_TERMIOS_LEN];
                read_bytes_from_user(arg, &mut raw)?;
                *self.termios.lock() = LinuxTermios { raw };
                if matches!(cmd.raw(), TCSETSW | TCSETSF) {
                    self.dev.flush().map_err(map_char_errno)?;
                }
                Ok(0)
            }
            TCSETS2 | TCSETSW2 | TCSETSF2 => {
                let mut raw = [0u8; LINUX_TERMIOS2_LEN];
                read_bytes_from_user(arg, &mut raw)?;
                let mut termios = [0u8; LINUX_TERMIOS_LEN];
                termios.copy_from_slice(&raw[..LINUX_TERMIOS_LEN]);
                *self.termios.lock() = LinuxTermios { raw: termios };
                if matches!(cmd.raw(), TCSETSW2 | TCSETSF2) {
                    self.dev.flush().map_err(map_char_errno)?;
                }
                Ok(0)
            }
            TIOCGWINSZ => {
                let winsize = *self.winsize.lock();
                write_bytes_to_user(arg, &winsize.to_bytes())?;
                Ok(0)
            }
            TIOCSWINSZ => {
                let mut raw = [0u8; LINUX_WINSIZE_LEN];
                read_bytes_from_user(arg, &mut raw)?;
                *self.winsize.lock() = LinuxWinSize::from_bytes(raw);
                Ok(0)
            }
            FIONREAD | TIOCOUTQ => {
                write_u32_to_user(arg, 0)?;
                Ok(0)
            }
            TIOCGPGRP => {
                write_i32_to_user(arg, self.current_or_stored_pgrp()?)?;
                Ok(0)
            }
            TIOCSPGRP => {
                let pgid = read_i32_from_user(arg)?;
                if pgid <= 0 {
                    return Err(Errno::EINVAL);
                }
                *self.foreground_pgrp.lock() = pgid;
                Ok(0)
            }
            TIOCGSID => {
                write_i32_to_user(arg, sched_operation::getsid(0)?)?;
                Ok(0)
            }
            TIOCGETD => {
                write_i32_to_user(arg, 0)?;
                Ok(0)
            }
            TIOCSETD => {
                let discipline = read_i32_from_user(arg)?;
                if discipline == 0 {
                    Ok(0)
                } else {
                    Err(Errno::EINVAL)
                }
            }
            TCSBRK | TCSBRKP => {
                self.dev.flush().map_err(map_char_errno)?;
                Ok(0)
            }
            TCXONC | TCFLSH | TIOCEXCL | TIOCNXCL | TIOCSCTTY | TIOCNOTTY => Ok(0),
            FIONBIO => {
                self.nonblock
                    .store(read_i32_from_user(arg)? != 0, Ordering::Release);
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }
    fn release(&self) {}
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 字符设备 InodeOps ─────────

/// 字符设备节点的操作对象。
///
/// 直接持有 `CharDev` 句柄。句柄内部共享 active/gone 状态，设备解绑后旧 inode
/// 和已打开 fd 都会通过同一状态停止访问底层驱动。
struct DevCharOps {
    dev: CharDevice,
}

impl DevCharOps {
    fn dev(&self) -> CharDevice {
        self.dev.clone()
    }
}

impl InodeOps for DevCharOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if !self.dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        Ok(Box::new(CharDevFileOps::new(
            self.dev.clone(),
            opts.nonblock,
        )))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 块设备 InodeOps ─────────

/// 块设备节点的操作对象。
///
/// 持有 `Arc<BlockDev>`，`open()` 时同样无需查找。
struct DevBlockOps {
    dev: Arc<BlockDevice>,
}

struct BlockDevFileOps {
    dev: Arc<BlockDevice>,
    sync_writes: bool,
}

fn map_block_submit_err(err: BlockSubmitError) -> VfsError {
    match err {
        BlockSubmitError::Unsupported => VfsError::NotSupported,
        BlockSubmitError::ReadOnly => VfsError::ReadOnlyFilesystem,
        BlockSubmitError::QueueFull => VfsError::WouldBlock,
        BlockSubmitError::DeviceGone => VfsError::NoDevice,
        BlockSubmitError::OutOfMemory => VfsError::OutOfMemory,
        BlockSubmitError::InvalidRequest(_) => VfsError::InvalidArgument,
    }
}

fn map_block_io_err(err: BlockIoError) -> VfsError {
    match err {
        BlockIoError::MediaError => VfsError::Io,
        BlockIoError::Unavailable => VfsError::NoDevice,
        BlockIoError::Timeout => VfsError::TimedOut,
        BlockIoError::ReadOnly => VfsError::ReadOnlyFilesystem,
        BlockIoError::Unsupported => VfsError::NotSupported,
    }
}

fn boxed_zeroed(len: usize) -> VfsResult<Box<[u8]>> {
    let mut data = Vec::new();
    data.try_reserve(len).map_err(|_| VfsError::OutOfMemory)?;
    data.resize(len, 0);
    Ok(data.into_boxed_slice())
}

fn boxed_copy(buf: &[u8]) -> VfsResult<Box<[u8]>> {
    let mut data = Vec::new();
    data.try_reserve(buf.len())
        .map_err(|_| VfsError::OutOfMemory)?;
    data.extend_from_slice(buf);
    Ok(data.into_boxed_slice())
}

fn block_range_for_io(dev: &BlockDevice, offset: u64, len: usize) -> VfsResult<Option<BlockRange>> {
    if len == 0 {
        return Ok(None);
    }
    if offset == u64::MAX {
        return Err(VfsError::InvalidArgument);
    }
    let block_size = dev.geometry().logical_block_size().get() as u64;
    let len_u64 = u64::try_from(len).map_err(|_| VfsError::InvalidArgument)?;
    if !offset.is_multiple_of(block_size) || !len_u64.is_multiple_of(block_size) {
        return Err(VfsError::InvalidArgument);
    }
    let blocks = len_u64 / block_size;
    let blocks = u32::try_from(blocks).map_err(|_| VfsError::InvalidArgument)?;
    Ok(Some(BlockRange {
        lba: offset / block_size,
        blocks,
    }))
}

fn submit_block_sync(dev: &Arc<BlockDevice>, req: BlockIoRequest) -> VfsResult<BlockIoCompletion> {
    let done = Arc::new(AtomicBool::new(false));
    let slot = Arc::new(Spinlock::new(None));
    let done_for_completion = Arc::clone(&done);
    let slot_for_completion = Arc::clone(&slot);
    let completion: BlockCompletion = Box::new(move |completion| {
        *slot_for_completion.lock() = Some(completion);
        done_for_completion.store(true, Ordering::Release);
    });

    if let Err((err, _req, _completion)) = dev.submit(req, completion) {
        return Err(map_block_submit_err(err));
    }

    while !done.load(Ordering::Acquire) {
        dev.poll();
        core::hint::spin_loop();
    }

    slot.lock().take().ok_or(VfsError::Io)
}

impl FileOps for BlockDevFileOps {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let Some(range) = block_range_for_io(&self.dev, offset, buf.len())? else {
            return Ok(0);
        };
        let completion = submit_block_sync(
            &self.dev,
            BlockIoRequest::Read {
                range,
                buffer: boxed_zeroed(buf.len())?,
            },
        )?;
        completion.result.map_err(map_block_io_err)?;
        match completion.request {
            BlockIoRequest::Read { buffer, .. } => {
                buf.copy_from_slice(&buffer);
                Ok(buf.len())
            }
            _ => Err(VfsError::Io),
        }
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let Some(range) = block_range_for_io(&self.dev, offset, buf.len())? else {
            return Ok(0);
        };
        let completion = submit_block_sync(
            &self.dev,
            BlockIoRequest::Write {
                range,
                buffer: boxed_copy(buf)?,
                fua: false,
            },
        )?;
        completion.result.map_err(map_block_io_err)?;
        if self.sync_writes {
            let completion = submit_block_sync(&self.dev, BlockIoRequest::Flush)?;
            completion.result.map_err(map_block_io_err)?;
        }
        Ok(buf.len())
    }

    fn readdir(
        &self,
        _pos: u64,
        _sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        Err(VfsError::NotADirectory)
    }

    fn sync(&self) -> VfsResult<()> {
        if !self.dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        let completion = submit_block_sync(&self.dev, BlockIoRequest::Flush)?;
        completion.result.map_err(map_block_io_err)
    }

    fn poll(&self, _interest: PollEvents) -> PollEvents {
        if !self.dev.is_active() {
            return PollEvents::POLLERR.with(PollEvents::POLLHUP);
        }
        PollEvents::POLLIN.with(PollEvents::POLLOUT)
    }

    fn ioctl(&self, cmd: IoctlCmd, arg: usize) -> Result<usize, Errno> {
        if !self.dev.is_active() {
            return Err(Errno::ENODEV);
        }

        let geometry = self.dev.geometry();
        match cmd.raw() {
            BLKROGET => {
                let readonly = if self.dev.features().contains(BlockFeatures::READ_ONLY) {
                    1
                } else {
                    0
                };
                write_i32_to_user(arg, readonly)?;
                Ok(0)
            }
            BLKGETSIZE => {
                let bytes = geometry.capacity_bytes().ok_or(Errno::EINVAL)?;
                let sectors = usize::try_from(bytes / 512).map_err(|_| Errno::EINVAL)?;
                write_usize_to_user(arg, sectors)?;
                Ok(0)
            }
            BLKGETSIZE64 => {
                let bytes = geometry.capacity_bytes().ok_or(Errno::EINVAL)?;
                write_u64_to_user(arg, bytes)?;
                Ok(0)
            }
            BLKSSZGET => {
                write_u32_to_user(arg, geometry.logical_block_size().get())?;
                Ok(0)
            }
            BLKBSZGET => {
                write_usize_to_user(arg, geometry.logical_block_size().get() as usize)?;
                Ok(0)
            }
            BLKPBSZGET => {
                write_u32_to_user(arg, geometry.physical_block_size().get())?;
                Ok(0)
            }
            BLKIOMIN => {
                write_u32_to_user(arg, geometry.logical_block_size().get())?;
                Ok(0)
            }
            BLKIOOPT => {
                let optimal = self
                    .dev
                    .limits()
                    .optimal_blocks_per_io()
                    .map(|blocks| {
                        blocks
                            .get()
                            .saturating_mul(geometry.logical_block_size().get())
                    })
                    .unwrap_or(0);
                write_u32_to_user(arg, optimal)?;
                Ok(0)
            }
            BLKALIGNOFF | BLKDISCARDZEROES | BLKROTATIONAL => {
                write_i32_to_user(arg, 0)?;
                Ok(0)
            }
            BLKGETDISKSEQ => {
                write_u64_to_user(arg, 0)?;
                Ok(0)
            }
            BLKFLSBUF => {
                match self.sync() {
                    Ok(()) | Err(VfsError::NotSupported) => {}
                    Err(err) => return Err(err.to_errno()),
                }
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl InodeOps for DevBlockOps {
    fn lookup(&self, _inode: &Inode, _name: &str) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotADirectory)
    }

    fn open(
        &self,
        _inode: &Inode,
        opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        if !self.dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        Ok(Box::new(BlockDevFileOps {
            dev: Arc::clone(&self.dev),
            sync_writes: opts.sync,
        }))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 根目录 InodeOps ─────────

/// devtmpfs 根目录（`/dev`）的操作对象。
///
/// 维护 `user_name → Arc<Inode>` 映射，支持 `lookup` 和 `readdir`。
/// 设备节点的增删通过 [`DevTmpfsSuperblockOps`] 对外暴露。
pub struct DevRootOps {
    pub(crate) children: Spinlock<BTreeMap<String, Arc<Inode>>>,
}

impl DevRootOps {
    fn new() -> Self {
        Self {
            children: Spinlock::new(BTreeMap::new()),
        }
    }

    /// 返回当前子节点的快照：`(user_name, Arc<Inode>)` 列表。
    pub fn children_snapshot(&self) -> alloc::vec::Vec<(String, Arc<Inode>)> {
        self.children
            .lock()
            .iter()
            .map(|(name, inode)| (name.clone(), Arc::clone(inode)))
            .collect()
    }
}

impl InodeOps for DevRootOps {
    fn lookup(&self, _inode: &Inode, name: &str) -> VfsResult<Arc<Inode>> {
        self.children
            .lock()
            .get(name)
            .cloned()
            .ok_or(VfsError::NotFound)
    }

    fn open(
        &self,
        _inode: &Inode,
        _opts: &OpenOptions,
        _cred: &Credentials,
    ) -> VfsResult<Box<dyn FileOps + Send + Sync>> {
        Ok(Box::new(DevRootFile {
            snapshot: self
                .children
                .lock()
                .iter()
                .map(|(name, inode)| DirEntry {
                    ino: inode.ino(),
                    name: SmallStr::new(name),
                    kind: inode.kind(),
                })
                .collect(),
        }))
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── 根目录 FileOps ─────────

struct DevRootFile {
    snapshot: alloc::vec::Vec<DirEntry>,
}

impl FileOps for DevRootFile {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::IsADirectory)
    }

    fn readdir(
        &self,
        pos: u64,
        sink: &mut dyn FnMut(DirEntry) -> ControlFlow<()>,
    ) -> VfsResult<u64> {
        let start = pos as usize;
        for (i, entry) in self.snapshot.iter().enumerate().skip(start) {
            if sink(entry.clone()).is_break() {
                return Ok(i as u64);
            }
        }
        Ok(self.snapshot.len() as u64)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn poll(&self, _interest: PollEvents) -> PollEvents {
        PollEvents(0)
    }

    fn release(&self) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── SuperblockOps ─────────

/// devtmpfs 超级块操作对象。
///
/// 同时提供公开的 `bind_char` / `bind_block` / `unbind` API，
/// 让设备驱动在设备注册/注销时同步更新 `/dev` 下的节点。
pub struct DevTmpfsSuperblockOps {
    next_ino: AtomicU64,
    /// 指向根目录 ops 的引用，用于 bind/unbind 时修改 children 表
    root_ops: Arc<DevRootOps>,
    /// 超级块弱引用，创建 Inode 时需要
    sb: vfs::sync::Spinlock<Option<alloc::sync::Weak<Superblock>>>,
}

impl DevTmpfsSuperblockOps {
    fn alloc_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    fn fs_id(&self) -> Option<FsId> {
        self.sb.lock().as_ref()?.upgrade().map(|sb| sb.fs_id)
    }

    fn sb_weak(&self) -> Option<alloc::sync::Weak<Superblock>> {
        self.sb.lock().clone()
    }

    /// 将字符设备绑定到 `/dev/<user_name>`。
    ///
    /// - `user_name`：用户空间可见的节点名称（如 `"uart0"`）
    /// - `dev`：已注册的字符设备对象（直接存入 inode，不再保存名称）
    pub fn bind_char(&self, user_name: &str, dev: CharDevice) -> VfsResult<()> {
        if !dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;

        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 1,
            mode: FileMode::new(0o660),
            uid: Uid::ROOT,
            gid: Gid::ROOT,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let ops = Arc::new(DevCharOps { dev });
        let inode = Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            FileType::CharDevice,
            DevId::new(0, 0),
            512,
            None,
            meta,
            ops,
            sb_weak,
        );

        let mut children = self.root_ops.children.lock();
        if children.contains_key(user_name) {
            return Err(VfsError::AlreadyExists);
        }
        children.insert(String::from(user_name), inode);
        Ok(())
    }

    /// 将块设备绑定到 `/dev/<user_name>`。
    ///
    /// - `user_name`：用户空间可见的节点名称（如 `"vda"`）
    /// - `dev`：已注册的块设备对象（`Arc` 直接存入 inode）
    pub fn bind_block(&self, user_name: &str, dev: Arc<BlockDevice>) -> VfsResult<()> {
        if !dev.is_active() {
            return Err(VfsError::NoDevice);
        }
        let fs_id = self.fs_id().ok_or(VfsError::InvalidArgument)?;
        let sb_weak = self.sb_weak().ok_or(VfsError::InvalidArgument)?;

        let now = Timespec::now();
        let meta = InodeMeta {
            size: 0,
            nlink: 1,
            mode: FileMode::new(0o660),
            uid: Uid::ROOT,
            gid: Gid::ROOT,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: 0,
        };

        let ops = Arc::new(DevBlockOps { dev });
        let inode = Inode::new(
            InodeId {
                fs_id,
                ino: self.alloc_ino(),
            },
            FileType::BlockDevice,
            DevId::new(0, 0),
            512,
            None,
            meta,
            ops,
            sb_weak,
        );

        let mut children = self.root_ops.children.lock();
        if children.contains_key(user_name) {
            return Err(VfsError::AlreadyExists);
        }
        children.insert(String::from(user_name), inode);
        Ok(())
    }

    /// 解除设备绑定，删除 `/dev/<user_name>` 节点。
    pub fn unbind(&self, user_name: &str) -> VfsResult<()> {
        let inode = self
            .root_ops
            .children
            .lock()
            .remove(user_name)
            .ok_or(VfsError::NotFound)?;

        if let Some(ops) = inode.downcast_ops::<DevCharOps>() {
            ops.dev.mark_gone();
        }
        if let Some(ops) = inode.downcast_ops::<DevBlockOps>() {
            ops.dev.mark_gone();
        }
        inode.set_nlink(0);
        inode.touch_ctime();
        if let Some(sb) = inode.superblock() {
            sb.remove_inode(inode.ino());
            if let Some(dentry) = vfs::DCACHE.get(&sb.root_dentry, user_name) {
                vfs::DCACHE.invalidate_dentry(&dentry);
                dentry.invalidate();
            }
        }
        Ok(())
    }

    /// 根据 `/dev/<user_name>` 节点名恢复其绑定的字符设备对象。
    pub fn char_dev(&self, user_name: &str) -> Option<CharDevice> {
        let inode = self.root_ops.children.lock().get(user_name).cloned()?;
        let ops = inode.downcast_ops::<DevCharOps>()?;
        let dev = ops.dev();
        dev.is_active().then_some(dev)
    }
}

impl SuperblockOps for DevTmpfsSuperblockOps {
    fn alloc_inode(&self, _sb: &Arc<Superblock>) -> VfsResult<Arc<Inode>> {
        Err(VfsError::NotSupported)
    }

    fn write_inode(&self, _inode: &Arc<Inode>) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self, sb: &Arc<Superblock>) -> VfsResult<FsStat> {
        Ok(FsStat {
            fs_type: 0x444f4445, // "devt" 魔数
            block_size: 512,
            total_blocks: 0,
            free_blocks: 0,
            avail_blocks: 0,
            total_inodes: self.next_ino.load(Ordering::Relaxed),
            free_inodes: 0,
            fs_id: sb.fs_id.raw(),
            name_max: 255,
        })
    }

    fn sync_fs(&self, _sb: &Arc<Superblock>) -> VfsResult<()> {
        Ok(())
    }

    fn remount(&self, _sb: &Arc<Superblock>, _flags: MountFlags) -> VfsResult<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

// ───────── FsDriver ─────────

/// devtmpfs 文件系统驱动。
///
/// 通过 `mount` 方法创建超级块，返回的 `Arc<Superblock>` 的
/// `ops` 字段可通过 `downcast_ops::<DevTmpfsSuperblockOps>()` 取回，
/// 供驱动调用 `bind_char` / `bind_block`。
///
/// # 典型初始化流程
///
/// ```rust,ignore
/// // 1. 挂载 devtmpfs 到 /dev
/// let sb = FS_REGISTRY.find("devtmpfs").unwrap().mount(None, "")?;
/// mount_ns.mount(&dev_dentry, &dev_mount, sb.clone(), MountFlags::empty())?;
///
/// // 2. 驱动注册后绑定设备
/// let ops = sb.downcast_ops::<DevTmpfsSuperblockOps>().unwrap();
/// ops.bind_char("uart0", char_dev)?;   // 直接绑定对象引用
/// ops.bind_block("vda", block_dev)?;   // 直接绑定 Arc<BlockDev>
/// ```
pub struct DevTmpfsDriver;

impl FsDriver for DevTmpfsDriver {
    fn name(&self) -> &'static str {
        "devtmpfs"
    }

    fn flags(&self) -> FsDriverFlags {
        FsDriverFlags::NODEV.with(FsDriverFlags::SINGLE)
    }

    fn mount(&self, _dev: Option<&str>, _data: &str) -> VfsResult<Arc<Superblock>> {
        let fs_id = FsId::new(DEVTMPFS_INSTANCE_COUNTER.fetch_add(1, Ordering::Relaxed));

        let root_ops = Arc::new(DevRootOps::new());

        // 只构造一个 DevTmpfsSuperblockOps 实例，move 进 new_cyclic 闭包，
        // 写入 weak ref 后再 Box 化存入 Superblock。外层不再持有任何引用，
        // 后续通过 sb.downcast_ops::<DevTmpfsSuperblockOps>() 访问。
        let sb_ops = DevTmpfsSuperblockOps {
            next_ino: AtomicU64::new(2),
            root_ops: Arc::clone(&root_ops),
            sb: vfs::sync::Spinlock::new(None),
        };

        let sb = Superblock::new(move |weak_sb| {
            sb_ops.sb.lock().replace(weak_sb.clone());

            let now = Timespec::now();
            let root_meta = InodeMeta {
                size: 0,
                nlink: 2,
                mode: FileMode::new(0o755),
                uid: Uid::ROOT,
                gid: Gid::ROOT,
                atime: now,
                mtime: now,
                ctime: now,
                blocks: 0,
            };

            let root_inode = Inode::new(
                InodeId { fs_id, ino: 1 },
                FileType::Directory,
                DevId::new(0, 0),
                512,
                None,
                root_meta,
                Arc::clone(&root_ops) as Arc<dyn InodeOps + Send + Sync>,
                weak_sb.clone(),
            );

            let root_dentry = Dentry::new_positive("", None, Arc::clone(&root_inode));

            Superblock {
                fs_type: "devtmpfs",
                fs_id,
                dev_id: None,
                block_size: 512,
                name_max: 255,
                root_inode,
                root_dentry,
                inode_cache: vfs::superblock::InodeCache::new(),
                ops: Box::new(sb_ops),
                self_weak: weak_sb,
            }
        });

        if let Some(ops) = sb.downcast_ops::<DevTmpfsSuperblockOps>() {
            let _ = ops.bind_char("null", CharDevice::null());
            let _ = ops.bind_char("zero", CharDevice::zero());
        }

        Ok(sb)
    }

    fn kill_sb(&self, _sb: Arc<Superblock>) {}

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
