//! 内核全局 console。
//!
//! console 本质上是一个 [`CharDev`]，无需额外 trait 包装。
//!
//! # 注册
//!
//! ```rust,ignore
//! register_console(dev);
//! ```
//!
//! # 线程安全
//!
//! 全局 console 保存 [`CharDev`] 句柄。句柄内部共享 active/gone 状态，因此设备解绑
//! 后，print/read 路径会自然停止访问底层驱动。

use core::fmt::{self, Write};

use spin::mutex::Mutex;

use crate::dev::char::CharDevice;

// ── 全局 console 设备 ────────────────────────────────────────────────────────

static CONSOLE: Mutex<Option<CharDevice>> = Mutex::new(None);

#[inline]
fn get_console() -> Option<CharDevice> {
    CONSOLE.lock().as_ref().cloned()
}

/// 将一个 [`CharDev`] 注册为内核全局 console。
#[kernel_symbols::export(
    name = "general.console.register_console",
    contract = "kernel.console.control@1",
    version = 1,
    capabilities = kernel_symbols::capability::HAL_CONTROL,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE,
    retained_args = 1 << 0
)]
pub fn register_console(dev: CharDevice) {
    *CONSOLE.lock() = Some(dev);
}

/// 取已注册的 console 字符设备。供 devtmpfs 把 `/dev/console` 重定向到当前
/// console，使用户态进程通过固定路径打开它。
#[kernel_symbols::export(name = "general.console.console_dev", contract = "kernel.console.query@1", version = 1, capabilities = kernel_symbols::capability::HAL_QUERY)]
pub fn console_dev() -> Option<CharDevice> {
    get_console()
}

// ── 公开 I/O 接口 ─────────────────────────────────────────────────────────────

/// 向 console 写入字节缓冲区（阻塞直到全部写入）。
///
/// console 未注册时为空操作。
#[kernel_symbols::export(name = "general.console.console_write", contract = "kernel.console.io@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn console_write(buf: &[u8]) {
    if let Some(dev) = get_console() {
        let _ = dev.write_all(buf);
    }
}

/// 从 console 读取最多 `buf.len()` 字节，返回实际读取数。
///
/// console 未注册或无可用数据时返回 0。
#[kernel_symbols::export(name = "general.console.console_read", contract = "kernel.console.io@1", version = 1, capabilities = kernel_symbols::capability::HAL_CONTROL, flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE)]
pub fn console_read(buf: &mut [u8]) -> usize {
    get_console()
        .and_then(|dev| dev.read(buf).ok())
        .unwrap_or(0)
}

// ── fmt::Write 集成 ────────────────────────────────────────────────────────────

struct ConsoleWriter;

impl Write for ConsoleWriter {
    /// 整串一次性写入，充分利用驱动 FIFO 批量传输能力。
    fn write_str(&mut self, s: &str) -> fmt::Result {
        console_write(s.as_bytes());
        Ok(())
    }
}

pub fn print(args: fmt::Arguments) {
    ConsoleWriter.write_fmt(args).unwrap();
}
