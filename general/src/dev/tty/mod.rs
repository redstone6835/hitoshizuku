//! TTY 核心层。
//!
//! 行规程(termios、规范/非规范输入、控制字符、前台进程组)与具体终端后端
//! 解耦:后端实现 [`TerminalDriver`],行规程统一由 [`TtyCore`] 承载。串口、
//! 虚拟终端(VT)与伪终端(pts)都通过同一套行规程获得一致的作业控制语义。
//!
//! 本层不感知 VFS:错误类型 [`TtyIoError`] 由投影层映射为 `VfsError`;
//! 设备身份仍由底层设备模型持有,这里只消费字节流与生命周期回调。

pub mod core;
pub mod pty;
pub mod vt;

pub use core::{
    CharDeviceTerminalDriver, TerminalDriver, TtyControlRequest, TtyControlResponse, TtyCore,
    TtyIoError, TtyIoResult, active_tty_cores, lookup_tty_core, shared_tty_core,
};
pub use pty::{
    PTY_MAX, PtyMasterFileOps, PtyPair, TIOCGPTN, TIOCGPTPEER, TIOCGPTLCK, TIOCSIG, TIOCSPTLCK,
    lookup_pair, note_pty_opened, open_ptmx, open_slave_file,
};
pub use vt::{VtDevice, VtManager, handle_vt_ioctl, vt_from_char_device};
