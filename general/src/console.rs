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
pub fn register_console(dev: CharDevice) {
    *CONSOLE.lock() = Some(dev);
}

// ── 公开 I/O 接口 ─────────────────────────────────────────────────────────────

/// 向 console 写入字节缓冲区（阻塞直到全部写入）。
///
/// console 未注册时为空操作。
#[inline]
pub fn console_write(buf: &[u8]) {
    if let Some(dev) = get_console() {
        let _ = dev.write_all(buf);
    }
}

/// 从 console 读取最多 `buf.len()` 字节，返回实际读取数。
///
/// console 未注册或无可用数据时返回 0。
#[inline]
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
