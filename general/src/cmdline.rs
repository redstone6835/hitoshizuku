//! 启动命令行的零分配解析。
//!
//! 不持有堆分配的内存，不依赖 allocator，可在启动最早期使用。
//! 只识别 `key=value` 条目；不带 `=` 的 flag 会被忽略。

use core::str;

/// 启动命令行视图。数据来源可以是静态缓冲区的 `&[u8]` 引用，
/// 也可以是固件传入的原始指针（通过 [`from_raw_until_nul`] 扫描到 NUL 终止符）。
pub struct Cmdline<'a> {
    text: &'a str,
}

impl<'a> Cmdline<'a> {
    /// 从 `&[u8]` 构造，NUL 之后的内容被忽略。
    pub fn new(bytes: &'a [u8]) -> Self {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let text = str::from_utf8(&bytes[..end]).unwrap_or("");
        Self { text }
    }

    /// 从原始指针构造：扫描到 NUL 或 `max` 字节为止。
    /// 适用于命令行尚未拷贝到内核静态缓冲区的早期启动阶段。
    ///
    /// # Safety
    /// `ptr` 必须指向可读的 ASCII 内存区域，且 `max` 不超过实际可读范围。
    pub unsafe fn from_raw_until_nul(ptr: *const u8, max: usize) -> Self {
        let mut len = 0usize;
        // Safety: 调用方保证 ptr 可读。
        while len < max && unsafe { *ptr.add(len) } != 0 {
            len += 1;
        }
        let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
        Self::new(bytes)
    }

    /// 查找指定键对应的值。重复键返回最后一个值。
    ///
    /// 空白分隔 token，单/双引号可以包裹含空白的值，独立的 `--` 之后不再
    /// 把内容视为内核参数。返回值直接借用原始命令行，因此只去掉成对的
    /// 外层引号，不展开反斜杠转义。
    pub fn find(&self, key: &str) -> Option<&'a str> {
        let mut found = None;
        let bytes = self.text.as_bytes();
        let key_bytes = key.as_bytes();
        let mut cursor = 0usize;

        while cursor < bytes.len() {
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor == bytes.len() {
                break;
            }

            let token_start = cursor;
            let mut quote = None;
            let mut equals = None;
            while cursor < bytes.len() {
                let byte = bytes[cursor];
                if let Some(expected) = quote {
                    if byte == expected {
                        quote = None;
                    }
                } else if byte == b'\'' || byte == b'"' {
                    quote = Some(byte);
                } else if byte == b'=' && equals.is_none() {
                    equals = Some(cursor);
                } else if byte.is_ascii_whitespace() {
                    break;
                }
                cursor += 1;
            }

            let token_end = cursor;
            if equals.is_none() && &bytes[token_start..token_end] == b"--" {
                break;
            }
            let Some(equals) = equals else {
                continue;
            };
            if &bytes[token_start..equals] != key_bytes {
                continue;
            }

            let value_start = equals + 1;
            let value = if value_start < token_end
                && (bytes[value_start] == b'\'' || bytes[value_start] == b'"')
                && token_end > value_start + 1
                && bytes[token_end - 1] == bytes[value_start]
            {
                &self.text[value_start + 1..token_end - 1]
            } else {
                &self.text[value_start..token_end]
            };
            found = Some(value);
        }
        found
    }

    /// 返回去掉 NUL 终止符后的原始命令行文本。
    ///
    /// 这里不做键值解析，供 `/sys/kernel/cmdline` 这类诊断视图直接展示启动器
    /// 交给内核的稳定快照；无效 UTF-8 在构造阶段已经被归一为空字符串。
    pub fn as_str(&self) -> &'a str {
        self.text
    }
}
