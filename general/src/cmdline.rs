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
}
