//! 启动命令行的零分配解析。
//!
//! 启动命令行由 loader 复制到静态固件缓冲区，内核启动阶段只需要按需查询少量
//! 键值。这里保留对原始字节的借用并线性扫描，避免在 allocator 尚处于早期热路径
//! 时为临时 cmdline 建表、分配和释放。

use core::str;

/// 启动命令行视图。
///
/// 只识别 `key=value` 条目；不带 `=` 的 flag 会被忽略。重复键按 Linux 习惯以后
/// 出现的值为准。
pub struct Cmdline<'a> {
    text: &'a str,
}

impl<'a> Cmdline<'a> {
    /// 从原始命令行字节构造借用视图。
    ///
    /// 若固件缓冲区意外包含终止 NUL，只解析 NUL 之前的内容。
    pub fn new(bytes: &'a [u8]) -> Self {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let text = str::from_utf8(&bytes[..end]).unwrap_or("");
        Self { text }
    }

    /// 查找指定键对应的值。重复键返回最后一个值。
    pub fn find(&self, key: &str) -> Option<&'a str> {
        let mut found = None;
        for item in self.text.split_ascii_whitespace() {
            let Some((item_key, value)) = item.split_once('=') else {
                continue;
            };
            if item_key == key {
                found = Some(value);
            }
        }
        found
    }

    /// 返回 `console=` 中的设备名部分。
    ///
    /// Linux 常见写法是 `console=ttyS0,115200n8`，逗号后的串口参数不是设备名。
    pub fn console_device(&self) -> Option<&'a str> {
        let raw = self.find("console")?;
        let dev = raw.split_once(',').map(|(dev, _)| dev).unwrap_or(raw);
        (!dev.is_empty()).then_some(dev)
    }
}
