//! klogctl/sys_syslog 支持。
//!
//! BusyBox `dmesg` 依赖这个 syscall 读取内核日志缓冲区。

use errno::Errno;
use general::mm::copy_to_user;
use general::syscall::SyscallContext;
use log::{self, LogLevel};

const SYSLOG_ACTION_CLOSE: usize = 0;
const SYSLOG_ACTION_OPEN: usize = 1;
const SYSLOG_ACTION_READ: usize = 2;
const SYSLOG_ACTION_READ_ALL: usize = 3;
const SYSLOG_ACTION_READ_CLEAR: usize = 4;
const SYSLOG_ACTION_CLEAR: usize = 5;
const SYSLOG_ACTION_CONSOLE_OFF: usize = 6;
const SYSLOG_ACTION_CONSOLE_ON: usize = 7;
const SYSLOG_ACTION_CONSOLE_LEVEL: usize = 8;
const SYSLOG_ACTION_SIZE_UNREAD: usize = 9;
const SYSLOG_ACTION_SIZE_BUFFER: usize = 10;

pub(super) fn sys_syslog(ctx: &mut SyscallContext<'_>) -> Result<usize, Errno> {
    let action = ctx.args[0];
    let buf = ctx.args[1];
    let len = ctx.args[2];

    match action {
        SYSLOG_ACTION_CLOSE | SYSLOG_ACTION_OPEN => Ok(0),
        SYSLOG_ACTION_CONSOLE_OFF => {
            log::LOGGER.disable_console();
            Ok(0)
        }
        SYSLOG_ACTION_CONSOLE_ON => {
            log::LOGGER.enable_console();
            Ok(0)
        }
        SYSLOG_ACTION_READ | SYSLOG_ACTION_READ_ALL | SYSLOG_ACTION_READ_CLEAR => {
            read_kernel_log(buf, len, action == SYSLOG_ACTION_READ_CLEAR)
        }
        SYSLOG_ACTION_CLEAR => {
            log::LOGGER.clear();
            Ok(0)
        }
        SYSLOG_ACTION_CONSOLE_LEVEL => set_console_level(len),
        SYSLOG_ACTION_SIZE_UNREAD => Ok(log::LOGGER.unread_len()),
        SYSLOG_ACTION_SIZE_BUFFER => Ok(log::LOGGER.capacity()),
        _ => Err(Errno::EINVAL),
    }
}

fn read_kernel_log(buf: usize, len: usize, clear: bool) -> Result<usize, Errno> {
    let data = log::LOGGER.export_text_limited(clear, len);
    if data.is_empty() || len == 0 {
        return Ok(0);
    }

    copy_to_user(buf, &data).map_err(|e| e.as_errno())?;
    Ok(data.len())
}

fn set_console_level(linux_level: usize) -> Result<usize, Errno> {
    // Linux klogctl action 8 accepts console loglevels 1..=8 and prints
    // messages whose priority is strictly lower than that value. Our internal
    // LogLevel is the printk priority itself, so store linux_level - 1.
    if !(1..=8).contains(&linux_level) {
        return Err(Errno::EINVAL);
    }

    let level = match linux_level - 1 {
        0 => LogLevel::Emergency,
        1 => LogLevel::Alert,
        2 => LogLevel::Critical,
        3 => LogLevel::Error,
        4 => LogLevel::Warning,
        5 => LogLevel::Notice,
        6 => LogLevel::Info,
        7 => LogLevel::Debug,
        _ => unreachable!(),
    };
    log::set_console_level(level);
    Ok(0)
}
