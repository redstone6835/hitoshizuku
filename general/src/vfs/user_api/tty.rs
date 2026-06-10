//! TTY 用户接口适配层。
//!
//! 终端行规程内部可以使用 typed `CharControlRequest`，但 termios/winsize 布局、
//! ioctl 号和控制字符默认值都属于用户态 ABI。集中在本模块后，devtmpfs 核心
//! 只需要把字符设备 inode 委托给 TTY 适配器，不再散落这些常量。

use errno::Errno;
use vfs::file::IoctlCmd;

use crate::dev::char::{CharControlRequest, CharControlResponse, CharDevice};
use crate::dev::control::ControlError;

use super::ioctl::{
    put_u32, read_bytes_from_user, read_i32_from_user, read_u32, write_bytes_to_user,
    write_i32_to_user, write_u32_to_user,
};

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

const TCIFLUSH: usize = 0;
const TCOFLUSH: usize = 1;
const TCIOFLUSH: usize = 2;
const TTY_DEFAULT_BREAK_MS: u32 = 250;
const TTY_BREAK_UNIT_MS: u32 = 100;

pub const LINUX_TERMIOS_LEN: usize = 36;
const LINUX_TERMIOS2_LEN: usize = 44;
const LINUX_WINSIZE_LEN: usize = 8;
const NCCS_OFFSET: usize = 17;
const NCCS_VINTR: usize = 0;
const NCCS_VQUIT: usize = 1;
const NCCS_VERASE: usize = 2;
const NCCS_VKILL: usize = 3;
const NCCS_VEOF: usize = 4;
const NCCS_VTIME: usize = 5;
const NCCS_VMIN: usize = 6;
const NCCS_VSUSP: usize = 10;

const ICRNL: u32 = 0x0100;
const IXON: u32 = 0x0400;
const OPOST: u32 = 0x0001;
const ONLCR: u32 = 0x0004;
const ISIG: u32 = 0x0001;
const ICANON: u32 = 0x0002;
const ECHO: u32 = 0x0008;
const ECHOE: u32 = 0x0010;
const ECHOK: u32 = 0x0020;

/// 用户 ABI 形态的 termios 快照。
#[derive(Clone, Copy)]
pub struct UserTermios {
    raw: [u8; LINUX_TERMIOS_LEN],
}

impl UserTermios {
    /// 构造控制台默认 termios。
    pub fn new_default() -> Self {
        let mut raw = [0u8; LINUX_TERMIOS_LEN];
        let _ = put_u32(&mut raw, 0, 0x0500); // ICRNL | IXON
        let _ = put_u32(&mut raw, 4, 0x0005); // OPOST | ONLCR
        let _ = put_u32(&mut raw, 8, 0x04bf); // B38400 | CS8 | CREAD | HUPCL
        let _ = put_u32(&mut raw, 12, 0x803b); // ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN
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
        for (dst, src) in out.iter_mut().zip(self.raw.iter()) {
            *dst = *src;
        }
        let _ = put_u32(&mut out, 36, 38400);
        let _ = put_u32(&mut out, 40, 38400);
        out
    }

    pub fn raw(&self) -> &[u8; LINUX_TERMIOS_LEN] {
        &self.raw
    }

    fn iflag(&self) -> u32 {
        read_u32(&self.raw, 0).unwrap_or(0)
    }

    fn oflag(&self) -> u32 {
        read_u32(&self.raw, 4).unwrap_or(0)
    }

    fn lflag(&self) -> u32 {
        read_u32(&self.raw, 12).unwrap_or(0)
    }

    fn cc(&self, index: usize) -> u8 {
        self.raw[NCCS_OFFSET + index]
    }

    pub fn canonical(&self) -> bool {
        (self.lflag() & ICANON) != 0
    }

    pub fn echo(&self) -> bool {
        (self.lflag() & ECHO) != 0
    }

    pub fn echoe(&self) -> bool {
        (self.lflag() & ECHOE) != 0
    }

    pub fn echok(&self) -> bool {
        (self.lflag() & ECHOK) != 0
    }

    fn isig(&self) -> bool {
        (self.lflag() & ISIG) != 0
    }

    pub fn icrnl(&self) -> bool {
        (self.iflag() & ICRNL) != 0
    }

    pub fn ixon(&self) -> bool {
        (self.iflag() & IXON) != 0
    }

    pub fn opost_onlcr(&self) -> bool {
        (self.oflag() & (OPOST | ONLCR)) == (OPOST | ONLCR)
    }

    pub fn vintr(&self) -> u8 {
        self.cc(NCCS_VINTR)
    }

    pub fn vquit(&self) -> u8 {
        self.cc(NCCS_VQUIT)
    }

    pub fn verase(&self) -> u8 {
        self.cc(NCCS_VERASE)
    }

    pub fn vkill(&self) -> u8 {
        self.cc(NCCS_VKILL)
    }

    pub fn veof(&self) -> u8 {
        self.cc(NCCS_VEOF)
    }

    pub fn vtime(&self) -> u8 {
        self.cc(NCCS_VTIME)
    }

    pub fn vmin(&self) -> u8 {
        self.cc(NCCS_VMIN)
    }

    pub fn vsusp(&self) -> u8 {
        self.cc(NCCS_VSUSP)
    }

    pub fn signal_for_input(&self, ch: u8) -> Option<sched::SignalNumber> {
        if !self.isig() || ch == 0 {
            return None;
        }
        if ch == self.vintr() {
            Some(sched::SignalNumber::SIGINT)
        } else if ch == self.vquit() {
            Some(sched::SignalNumber::SIGQUIT)
        } else if ch == self.vsusp() {
            Some(sched::SignalNumber::SIGTSTP)
        } else {
            None
        }
    }
}

/// 用户 ABI 形态的 winsize 快照。
#[derive(Clone, Copy)]
pub struct UserWinSize {
    rows: u16,
    cols: u16,
    xpixel: u16,
    ypixel: u16,
}

impl UserWinSize {
    pub const fn default_console() -> Self {
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
        let rows = self.rows.to_le_bytes();
        let cols = self.cols.to_le_bytes();
        let xpixel = self.xpixel.to_le_bytes();
        let ypixel = self.ypixel.to_le_bytes();
        out[0] = rows[0];
        out[1] = rows[1];
        out[2] = cols[0];
        out[3] = cols[1];
        out[4] = xpixel[0];
        out[5] = xpixel[1];
        out[6] = ypixel[0];
        out[7] = ypixel[1];
        out
    }
}

/// TTY ioctl 处理时需要访问的共享状态。
pub trait TtyIoctlState {
    fn termios(&self) -> UserTermios;
    fn set_termios(&self, termios: UserTermios);
    fn winsize(&self) -> UserWinSize;
    fn set_winsize(&self, winsize: UserWinSize);
    fn clear_line_state(&self);
    fn foreground_pgrp(&self) -> i32;
    fn set_foreground_pgrp(&self, pgrp: i32);
}

/// TTY ioctl 外部上下文。
pub trait TtyIoctlContext {
    fn current_or_stored_pgrp(&self) -> Result<i32, Errno>;
    fn session_id(&self) -> Result<i32, Errno>;
}

/// 判断 ioctl 是否是当前适配层认识的 TTY 命令。
pub fn is_tty_ioctl(cmd: IoctlCmd) -> bool {
    matches!(
        cmd.raw(),
        TCGETS
            | TCSETS
            | TCSETSW
            | TCSETSF
            | TCSBRK
            | TCXONC
            | TCFLSH
            | TIOCEXCL
            | TIOCNXCL
            | TIOCSCTTY
            | TIOCGPGRP
            | TIOCSPGRP
            | TIOCOUTQ
            | TIOCGWINSZ
            | TIOCSWINSZ
            | FIONREAD
            | TIOCNOTTY
            | TIOCSETD
            | TIOCGETD
            | TCSBRKP
            | TIOCGSID
            | TCGETS2
            | TCSETS2
            | TCSETSW2
            | TCSETSF2
    )
}

/// 执行 TTY ioctl，把用户 ABI 转换为 typed char control 或状态更新。
pub fn handle_tty_ioctl<S, C>(
    state: &S,
    ctx: &C,
    dev: &CharDevice,
    cmd: IoctlCmd,
    arg: usize,
) -> Result<usize, Errno>
where
    S: TtyIoctlState,
    C: TtyIoctlContext,
{
    match cmd.raw() {
        TCGETS => {
            let termios = state.termios();
            write_bytes_to_user(arg, termios.raw())?;
            Ok(0)
        }
        TCGETS2 => {
            let termios = state.termios();
            write_bytes_to_user(arg, &termios.as_termios2_bytes())?;
            Ok(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            let mut raw = [0u8; LINUX_TERMIOS_LEN];
            read_bytes_from_user(arg, &mut raw)?;
            state.set_termios(UserTermios { raw });
            if matches!(cmd.raw(), TCSETSW | TCSETSF) {
                control_done_ignore_unsupported(dev, CharControlRequest::DrainTx)?;
            }
            if matches!(cmd.raw(), TCSETSF) {
                state.clear_line_state();
                control_done_ignore_unsupported(dev, CharControlRequest::FlushRx)?;
            }
            Ok(0)
        }
        TCSETS2 | TCSETSW2 | TCSETSF2 => {
            let mut raw = [0u8; LINUX_TERMIOS2_LEN];
            read_bytes_from_user(arg, &mut raw)?;
            let mut termios = [0u8; LINUX_TERMIOS_LEN];
            for (dst, src) in termios.iter_mut().zip(raw.iter()) {
                *dst = *src;
            }
            state.set_termios(UserTermios { raw: termios });
            sync_termios2_hardware(dev, &raw)?;
            if matches!(cmd.raw(), TCSETSW2 | TCSETSF2) {
                control_done_ignore_unsupported(dev, CharControlRequest::DrainTx)?;
            }
            if matches!(cmd.raw(), TCSETSF2) {
                state.clear_line_state();
                control_done_ignore_unsupported(dev, CharControlRequest::FlushRx)?;
            }
            Ok(0)
        }
        TIOCGWINSZ => {
            write_bytes_to_user(arg, &state.winsize().to_bytes())?;
            Ok(0)
        }
        TIOCSWINSZ => {
            let mut raw = [0u8; LINUX_WINSIZE_LEN];
            read_bytes_from_user(arg, &mut raw)?;
            state.set_winsize(UserWinSize::from_bytes(raw));
            Ok(0)
        }
        FIONREAD => {
            let queued = control_u32_or_zero(dev, CharControlRequest::GetInputQueueLen)?;
            write_u32_to_user(arg, queued)?;
            Ok(0)
        }
        TIOCOUTQ => {
            let queued = control_u32_or_zero(dev, CharControlRequest::GetOutputQueueLen)?;
            write_u32_to_user(arg, queued)?;
            Ok(0)
        }
        TIOCGPGRP => {
            write_i32_to_user(arg, ctx.current_or_stored_pgrp()?)?;
            Ok(0)
        }
        TIOCSPGRP => {
            let pgid = read_i32_from_user(arg)?;
            if pgid <= 0 {
                return Err(Errno::EINVAL);
            }
            state.set_foreground_pgrp(pgid);
            Ok(0)
        }
        TIOCGSID => {
            write_i32_to_user(arg, ctx.session_id()?)?;
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
        TCSBRK => {
            control_done_ignore_unsupported(dev, CharControlRequest::DrainTx)?;
            if arg == 0 {
                control_done_ignore_unsupported(
                    dev,
                    CharControlRequest::SendBreak {
                        duration_ms: TTY_DEFAULT_BREAK_MS,
                    },
                )?;
            }
            Ok(0)
        }
        TCSBRKP => {
            control_done_ignore_unsupported(dev, CharControlRequest::DrainTx)?;
            let units = u32::try_from(arg).unwrap_or(u32::MAX);
            let duration_ms = if units == 0 {
                TTY_DEFAULT_BREAK_MS
            } else {
                units.saturating_mul(TTY_BREAK_UNIT_MS)
            };
            control_done_ignore_unsupported(dev, CharControlRequest::SendBreak { duration_ms })?;
            Ok(0)
        }
        TCFLSH => match arg {
            TCIFLUSH => {
                state.clear_line_state();
                control_done_ignore_unsupported(dev, CharControlRequest::FlushRx)?;
                Ok(0)
            }
            TCOFLUSH => {
                control_done_ignore_unsupported(dev, CharControlRequest::FlushTx)?;
                Ok(0)
            }
            TCIOFLUSH => {
                state.clear_line_state();
                control_done_ignore_unsupported(dev, CharControlRequest::FlushBoth)?;
                Ok(0)
            }
            _ => Err(Errno::EINVAL),
        },
        TCXONC | TIOCEXCL | TIOCNXCL | TIOCSCTTY | TIOCNOTTY => Ok(0),
        _ => Err(Errno::ENOTTY),
    }
}

fn sync_termios2_hardware(dev: &CharDevice, raw: &[u8; LINUX_TERMIOS2_LEN]) -> Result<(), Errno> {
    let ospeed = read_u32(raw, 40).ok_or(Errno::EINVAL)?;
    if ospeed == 0 {
        return Ok(());
    }
    control_done_ignore_unsupported(dev, CharControlRequest::SetSerialConfig { baud: Some(ospeed) })
}

fn control_done_ignore_unsupported(dev: &CharDevice, req: CharControlRequest) -> Result<(), Errno> {
    match dev.control(req) {
        Ok(CharControlResponse::Done) | Err(ControlError::Unsupported) => Ok(()),
        Ok(_) => Err(Errno::EINVAL),
        Err(err) => Err(map_control_errno(err)),
    }
}

fn control_u32_or_zero(dev: &CharDevice, req: CharControlRequest) -> Result<u32, Errno> {
    match dev.control(req) {
        Ok(CharControlResponse::U32(value)) => Ok(value),
        Ok(CharControlResponse::Done) => Err(Errno::EINVAL),
        Err(ControlError::Unsupported) => Ok(0),
        Err(err) => Err(map_control_errno(err)),
    }
}

fn map_control_errno(e: ControlError) -> Errno {
    match e {
        ControlError::Unsupported => Errno::ENOTTY,
        ControlError::Invalid => Errno::EINVAL,
        ControlError::NoDevice => Errno::ENODEV,
        ControlError::Busy => Errno::EBUSY,
        ControlError::Io => Errno::EIO,
        ControlError::Permission => Errno::EPERM,
    }
}
