#![no_std]

extern crate alloc;

/// 内核日志系统，使用环形缓冲区存储。
///
/// 该模块提供了一个高效的日志系统，使用固定大小的环形缓冲区存储日志条目。
/// 所有日志都会写入环形缓冲区；控制台/sink 的输出再按当前日志级别过滤。
/// 提供类似 `dmesg` 的日志读取接口，并支持运行时热切换输出级别。
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::fmt;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

// ============================================================================
// 日志级别定义
// ============================================================================

/// 日志级别，与 Linux 内核日志级别对应
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum LogLevel {
    /// KERN_EMERG: 系统不可用
    Emergency = 0,
    /// KERN_ALERT: 必须立即采取行动
    Alert = 1,
    /// KERN_CRIT: 临界条件
    Critical = 2,
    /// KERN_ERR: 错误条件
    Error = 3,
    /// KERN_WARNING: 警告条件
    Warning = 4,
    /// KERN_NOTICE: 正常但重要的条件
    Notice = 5,
    /// KERN_INFO: 信息性消息
    Info = 6,
    /// KERN_DEBUG: 调试级别消息
    Debug = 7,
}

impl LogLevel {
    /// 获取日志级别的字符串表示
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Emergency => "EMERG",
            LogLevel::Alert => "ALERT",
            LogLevel::Critical => "CRIT",
            LogLevel::Error => "ERR",
            LogLevel::Warning => "WARN",
            LogLevel::Notice => "NOTICE",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
        }
    }

    /// 从数值转换为 LogLevel
    fn from_u8(value: u8) -> Self {
        match value {
            0 => LogLevel::Emergency,
            1 => LogLevel::Alert,
            2 => LogLevel::Critical,
            3 => LogLevel::Error,
            4 => LogLevel::Warning,
            5 => LogLevel::Notice,
            6 => LogLevel::Info,
            7 => LogLevel::Debug,
            _ => LogLevel::Info,
        }
    }
}

// ============================================================================
// 常量定义
// ============================================================================

/// 日志条目的最大长度（字节）
const MAX_LOG_ENTRY_LEN: usize = 1024;
/// 格式化后的日志消息最大长度（不含头部）
const MAX_LOG_MESSAGE_LEN: usize = MAX_LOG_ENTRY_LEN - core::mem::size_of::<LogEntry>();

/// 环形缓冲区的大小（字节）
/// 默认 256 KiB，与 Linux 内核日志缓冲区大小相当
const LOG_BUFFER_SIZE: usize = 256 * 1024;

// ============================================================================
// 日志条目结构
// ============================================================================

/// 日志条目结构
#[repr(C)]
#[derive(Clone, Copy)]
struct LogEntry {
    /// 时间戳（纳秒）
    timestamp: u64,
    /// 日志级别
    level: u8,
    /// 日志序列号
    seq: u64,
    /// 日志消息长度
    len: u16,
}

#[derive(Clone, Copy)]
struct ParsedLogEntry {
    header: LogEntry,
    total_size: usize,
}

impl LogEntry {
    /// 计算该条目占用的总字节数（包括消息内容）
    #[inline]
    fn total_size(&self) -> usize {
        core::mem::size_of::<Self>() + self.len as usize
    }

    /// 获取条目头部大小
    #[inline]
    fn header_size() -> usize {
        core::mem::size_of::<Self>()
    }
}

// ============================================================================
// 环形缓冲区日志系统
// ============================================================================

/// 环形缓冲区日志系统
pub struct LogBuffer {
    /// 环形缓冲区数据
    buffer: UnsafeCell<[u8; LOG_BUFFER_SIZE]>,
    /// 写入位置（字节偏移）
    write_pos: AtomicUsize,
    /// 读取位置（字节偏移）
    read_pos: AtomicUsize,
    /// 日志序列号
    seq: AtomicUsize,
    /// 当前日志级别阈值（只分发 <= 该级别的日志到 sink）
    log_level: AtomicUsize,
    /// 非阻塞写锁：避免单核中断重入把 ring buffer 写坏
    write_lock: AtomicUsize,
    /// 是否禁用日志 sink 分发
    sink_disabled: AtomicUsize,
    /// 已绑定的日志 sink
    sink: AtomicPtr<LogSink>,
}

// 早期启动阶段是单核串行初始化，这里允许静态持有内部可变状态。
unsafe impl Sync for LogBuffer {}

impl LogBuffer {
    /// 创建一个新的日志缓冲区
    pub const fn new() -> Self {
        Self {
            buffer: UnsafeCell::new([0u8; LOG_BUFFER_SIZE]),
            write_pos: AtomicUsize::new(0),
            read_pos: AtomicUsize::new(0),
            seq: AtomicUsize::new(0),
            log_level: AtomicUsize::new(LogLevel::Info as usize),
            write_lock: AtomicUsize::new(0),
            sink_disabled: AtomicUsize::new(0),
            sink: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// 设置 sink 输出级别阈值。
    ///
    /// 该阈值只影响打印/分发，不影响环形缓冲区收集；
    /// 缓冲区始终记录全部日志，可在运行时热切换。
    pub fn set_log_level(&self, level: LogLevel) {
        self.log_level.store(level as usize, Ordering::Release);
    }

    /// 获取当前 sink 输出级别阈值
    pub fn get_log_level(&self) -> LogLevel {
        let level = self.log_level.load(Ordering::Acquire);
        LogLevel::from_u8(level as u8)
    }

    /// 兼容旧接口：设置控制台输出级别阈值。
    ///
    /// 当前 console/sink 共用同一个热切换级别。
    pub fn set_console_level(&self, level: LogLevel) {
        self.set_log_level(level);
    }

    /// 兼容旧接口：获取控制台输出级别阈值。
    pub fn get_console_level(&self) -> LogLevel {
        self.get_log_level()
    }

    /// 绑定日志 sink
    pub fn bind_sink(&self, sink: &'static LogSink) {
        self.sink
            .store(sink as *const LogSink as *mut LogSink, Ordering::Release);
    }

    /// 清除日志 sink
    pub fn clear_sink(&self) {
        self.sink.store(ptr::null_mut(), Ordering::Release);
    }

    /// 禁用日志 sink 分发
    pub fn disable_sink(&self) {
        self.sink_disabled.store(1, Ordering::Release);
    }

    /// 启用日志 sink 分发
    pub fn enable_sink(&self) {
        self.sink_disabled.store(0, Ordering::Release);
    }

    /// 兼容旧接口：禁用日志 sink 分发
    pub fn disable_console(&self) {
        self.disable_sink();
    }

    /// 兼容旧接口：启用日志 sink 分发
    pub fn enable_console(&self) {
        self.enable_sink();
    }

    /// 检查日志 sink 分发是否被禁用
    #[inline]
    fn is_sink_disabled(&self) -> bool {
        self.sink_disabled.load(Ordering::Acquire) != 0
    }

    #[inline]
    fn should_dispatch_to_sink(&self, level: LogLevel) -> bool {
        level as usize <= self.log_level.load(Ordering::Acquire)
    }

    /// 写入日志条目
    fn write_entry(&self, level: LogLevel, timestamp: u64, message: &str) {
        let Some(_lock) = self.try_lock() else {
            return;
        };

        let msg_bytes = message.as_bytes();
        if msg_bytes.len() > u16::MAX as usize {
            return;
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed) as u64;
        let entry = LogEntry {
            timestamp,
            level: level as u8,
            seq,
            len: msg_bytes.len() as u16,
        };

        let entry_size = LogEntry::header_size();
        let total_size = entry_size + msg_bytes.len();

        // 如果消息太长，不记录
        if total_size > MAX_LOG_ENTRY_LEN {
            return;
        }

        let pos = self.write_pos.load(Ordering::Acquire);
        let read_pos = self.read_pos.load(Ordering::Acquire);
        let read_pos = self.discard_to_fit(pos, read_pos, total_size);
        self.read_pos.store(read_pos, Ordering::Release);

        self.write_bytes_at(pos, entry_as_bytes(&entry));
        self.write_bytes_at(pos + entry_size, msg_bytes);
        self.write_pos.store(pos + total_size, Ordering::Release);

        if !self.is_sink_disabled() && self.should_dispatch_to_sink(level) {
            self.dispatch_record(LogRecord {
                timestamp,
                level,
                seq,
                message,
            });
        }
    }

    fn dispatch_record(&self, record: LogRecord<'_>) {
        let sink_ptr = self.sink.load(Ordering::Acquire);
        if sink_ptr.is_null() {
            return;
        }
        let sink = unsafe { &*sink_ptr };
        (sink.write_record)(&record);
    }

    /// 丢弃最旧的日志条目以腾出空间。
    ///
    /// `write_pos`/`read_pos` 是单调增长的逻辑偏移，底层数组按 modulo 访问。
    /// 必须始终保证逻辑 unread 区间不超过 ring 容量，否则读端会把同一圈数据
    /// 重复解析成超大日志流。
    fn discard_to_fit(&self, write_pos: usize, mut read_pos: usize, needed_space: usize) -> usize {
        if write_pos.saturating_sub(read_pos) > LOG_BUFFER_SIZE {
            return write_pos;
        }

        while write_pos
            .saturating_sub(read_pos)
            .saturating_add(needed_space)
            > LOG_BUFFER_SIZE
        {
            let Some(entry) = self.parse_entry_at(read_pos, write_pos) else {
                return write_pos;
            };
            read_pos += entry.total_size;
        }

        read_pos
    }

    /// 获取所有日志（类似 dmesg）
    /// 返回一个迭代器，可以遍历所有日志条目
    pub fn read_all(&self) -> LogIterator<'_> {
        let mut entries = Vec::new();
        if let Some(_lock) = self.try_lock() {
            let mut pos = self.read_pos.load(Ordering::Acquire);
            let end_pos = self.write_pos.load(Ordering::Acquire);
            if end_pos.saturating_sub(pos) > LOG_BUFFER_SIZE {
                pos = end_pos;
                self.read_pos.store(pos, Ordering::Release);
            }
            while pos < end_pos {
                let Some(entry) = self.parse_entry_at(pos, end_pos) else {
                    break;
                };
                let msg_start = pos + LogEntry::header_size();
                let message = self.read_message_string(msg_start, entry.header.len as usize);
                entries.push(OwnedLogEntry {
                    timestamp: entry.header.timestamp,
                    level: LogLevel::from_u8(entry.header.level),
                    seq: entry.header.seq,
                    message,
                });
                pos += entry.total_size;
            }
        }
        LogIterator {
            _buffer: self,
            inner: entries.into_iter(),
        }
    }

    /// 清空日志缓冲区
    pub fn clear(&self) {
        let Some(_lock) = self.try_lock() else {
            return;
        };
        self.clear_locked();
    }

    pub fn unread_len(&self) -> usize {
        let read_pos = self.read_pos.load(Ordering::Acquire);
        let write_pos = self.write_pos.load(Ordering::Acquire);
        write_pos.saturating_sub(read_pos).min(LOG_BUFFER_SIZE)
    }

    pub const fn capacity(&self) -> usize {
        LOG_BUFFER_SIZE
    }

    pub fn export_text(&self, clear: bool) -> Vec<u8> {
        self.export_text_limited(clear, LOG_BUFFER_SIZE)
    }

    pub fn export_text_limited(&self, clear: bool, max_len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(max_len.min(LOG_BUFFER_SIZE));
        let Some(_lock) = self.try_lock() else {
            return out;
        };

        if max_len != 0 {
            let mut pos = self.read_pos.load(Ordering::Acquire);
            let end_pos = self.write_pos.load(Ordering::Acquire);
            if end_pos.saturating_sub(pos) > LOG_BUFFER_SIZE {
                pos = end_pos;
                self.read_pos.store(pos, Ordering::Release);
            }
            while pos < end_pos && out.len() < max_len {
                let Some(entry) = self.parse_entry_at(pos, end_pos) else {
                    break;
                };
                self.append_text_entry(&mut out, max_len, pos, entry);
                pos += entry.total_size;
            }
        }

        if clear {
            self.clear_locked();
        }
        out
    }

    fn clear_locked(&self) {
        self.write_pos.store(0, Ordering::Release);
        self.read_pos.store(0, Ordering::Release);
        self.seq.store(0, Ordering::Release);
    }

    fn append_text_entry(
        &self,
        out: &mut Vec<u8>,
        max_len: usize,
        pos: usize,
        entry: ParsedLogEntry,
    ) {
        let mut message_buf = [0u8; MAX_LOG_MESSAGE_LEN];
        let msg_len = (entry.header.len as usize).min(MAX_LOG_MESSAGE_LEN);
        self.read_bytes_at(pos + LogEntry::header_size(), &mut message_buf[..msg_len]);
        let message = core::str::from_utf8(&message_buf[..msg_len]).unwrap_or("<invalid utf8>");
        let (secs, nanos) = format_timestamp(entry.header.timestamp);
        let mut writer = LimitedVecWriter {
            out,
            limit: max_len,
        };
        let _ = fmt::write(
            &mut writer,
            format_args!("[{:6}.{:06}] {}\n", secs, nanos / 1000, message),
        );
    }

    fn try_lock(&self) -> Option<LogBufferGuard<'_>> {
        self.write_lock
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(LogBufferGuard { buffer: self })
    }

    fn write_bytes_at(&self, pos: usize, bytes: &[u8]) {
        unsafe {
            let buffer = &mut *self.buffer.get();
            for (offset, byte) in bytes.iter().enumerate() {
                buffer[(pos + offset) % LOG_BUFFER_SIZE] = *byte;
            }
        }
    }

    fn read_bytes_at(&self, pos: usize, out: &mut [u8]) {
        unsafe {
            let buffer = &*self.buffer.get();
            for (offset, byte) in out.iter_mut().enumerate() {
                *byte = buffer[(pos + offset) % LOG_BUFFER_SIZE];
            }
        }
    }

    fn parse_entry_at(&self, pos: usize, end_pos: usize) -> Option<ParsedLogEntry> {
        let header_size = LogEntry::header_size();
        if pos >= end_pos || end_pos - pos < header_size {
            return None;
        }

        let mut header_bytes = [0u8; core::mem::size_of::<LogEntry>()];
        self.read_bytes_at(pos, &mut header_bytes[..header_size]);
        let header = log_entry_from_bytes(&header_bytes);
        let total_size = header.total_size();

        if total_size < header_size || total_size > MAX_LOG_ENTRY_LEN || pos + total_size > end_pos
        {
            return None;
        }

        Some(ParsedLogEntry { header, total_size })
    }

    fn read_message_string(&self, pos: usize, len: usize) -> String {
        if len == 0 {
            return String::new();
        }
        let mut message_bytes = alloc::vec![0u8; len];
        self.read_bytes_at(pos, &mut message_bytes);
        String::from_utf8_lossy(&message_bytes).into_owned()
    }
}

// ============================================================================
// 日志迭代器
// ============================================================================

/// 日志迭代器
pub struct LogIterator<'a> {
    _buffer: &'a LogBuffer,
    inner: alloc::vec::IntoIter<OwnedLogEntry>,
}

/// 日志条目信息
#[derive(Clone, Copy)]
pub struct LogEntryInfo<'a> {
    pub timestamp: u64,
    pub level: LogLevel,
    pub seq: u64,
    pub message: &'a str,
}

/// 拥有消息副本的稳定日志条目视图
pub struct OwnedLogEntry {
    pub timestamp: u64,
    pub level: LogLevel,
    pub seq: u64,
    pub message: String,
}

/// 提供给日志 sink 的稳定记录视图
#[derive(Clone, Copy)]
pub struct LogRecord<'a> {
    pub timestamp: u64,
    pub level: LogLevel,
    pub seq: u64,
    pub message: &'a str,
}

/// 日志 sink 定义
#[derive(Clone, Copy)]
pub struct LogSink {
    pub write_record: for<'a> fn(&LogRecord<'a>),
}

impl<'a> Iterator for LogIterator<'a> {
    type Item = OwnedLogEntry;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

struct LimitedVecWriter<'a> {
    out: &'a mut Vec<u8>,
    limit: usize,
}

impl fmt::Write for LimitedVecWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let remaining = self.limit.saturating_sub(self.out.len());
        if remaining == 0 {
            return Ok(());
        }
        let copy_len = remaining.min(s.len());
        self.out.extend_from_slice(&s.as_bytes()[..copy_len]);
        Ok(())
    }
}

struct LogBufferGuard<'a> {
    buffer: &'a LogBuffer,
}

impl Drop for LogBufferGuard<'_> {
    fn drop(&mut self) {
        self.buffer.write_lock.store(0, Ordering::Release);
    }
}

// ============================================================================
// 全局日志缓冲区和写入器
// ============================================================================

/// 全局日志缓冲区实例
pub static LOGGER: LogBuffer = LogBuffer::new();

// ============================================================================
// 公共接口
// ============================================================================

/// 记录日志（内部接口）
pub fn logger_entry(level: LogLevel, timestamp: u64, message: &str) {
    LOGGER.write_entry(level, timestamp, message);
}

/// 记录格式化日志（零堆分配路径）。
#[kernel_symbols::export(
    name = "log.logger_entry_fmt",
    contract = "kernel.log.write@1",
    version = 1,
    capabilities = kernel_symbols::capability::CORE_SAFE,
    flags = kernel_symbols::KERNEL_SYMBOL_FLAG_MUTATES_STATE
)]
pub fn logger_entry_fmt(level: LogLevel, timestamp: u64, args: fmt::Arguments<'_>) {
    let mut buf = StackMessageBuffer::<MAX_LOG_MESSAGE_LEN>::new();
    let _ = fmt::write(&mut buf, args);
    LOGGER.write_entry(level, timestamp, buf.as_str());
}

/// 绑定全局日志 sink
pub fn bind_log_sink(sink: &'static LogSink) {
    LOGGER.bind_sink(sink);
}

/// 清除全局日志 sink
pub fn clear_log_sink() {
    LOGGER.clear_sink();
}

/// 设置当前 sink 输出级别。
///
/// 所有日志始终写入环形缓冲区；该接口只影响打印/分发，支持运行时热切换。
pub fn set_log_level(level: LogLevel) {
    LOGGER.set_log_level(level);
}

/// 获取当前 sink 输出级别。
pub fn get_log_level() -> LogLevel {
    LOGGER.get_log_level()
}

/// 兼容旧接口：设置 console 输出级别。
pub fn set_console_level(level: LogLevel) {
    LOGGER.set_console_level(level);
}

/// 兼容旧接口：获取 console 输出级别。
pub fn get_console_level() -> LogLevel {
    LOGGER.get_console_level()
}

static TIMESTAMP_SOURCE: AtomicUsize = AtomicUsize::new(0);

/// 注册日志时间戳来源（纳秒）。
pub fn register_timestamp_source(source: fn() -> u64) {
    TIMESTAMP_SOURCE.store(source as usize, Ordering::Release);
}

/// 获取当前时间戳（纳秒）
/// 这个函数会被 printk 宏调用来获取时间戳
#[kernel_symbols::export(
    name = "log.get_timestamp_ns",
    contract = "kernel.log.timestamp@1",
    version = 1,
    capabilities = kernel_symbols::capability::CORE_SAFE
)]
pub fn get_timestamp_ns() -> u64 {
    let source = TIMESTAMP_SOURCE.load(Ordering::Acquire);
    if source == 0 {
        return 0;
    }
    let callback: fn() -> u64 = unsafe { core::mem::transmute(source) };
    callback()
}

/// 格式化时间戳为 Linux 风格的日志前缀
/// 例如: [    0.123456]
pub fn format_timestamp(ns: u64) -> (u64, u32) {
    let secs = ns / 1_000_000_000;
    let nanos = (ns % 1_000_000_000) as u32;
    (secs, nanos)
}

fn entry_as_bytes(entry: &LogEntry) -> &[u8] {
    unsafe {
        core::slice::from_raw_parts(
            entry as *const LogEntry as *const u8,
            core::mem::size_of::<LogEntry>(),
        )
    }
}

fn log_entry_from_bytes(bytes: &[u8; core::mem::size_of::<LogEntry>()]) -> LogEntry {
    unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const LogEntry) }
}

// ============================================================================
// 日志宏
// ============================================================================

/// printk 宏 - 目前默认记录 Info 级别日志
#[macro_export]
macro_rules! printk {
    ($($arg:tt)*) => {{
        let timestamp_ns = $crate::get_timestamp_ns();
        $crate::logger_entry_fmt($crate::LogLevel::Info, timestamp_ns, ::core::format_args!($($arg)*))
    }};
}

/// 记录 Emergency 级别日志
#[macro_export]
macro_rules! emergency {
    ($($arg:tt)*) => {
        $crate::logger_entry_fmt($crate::LogLevel::Emergency, $crate::get_timestamp_ns(), ::core::format_args!($($arg)*))
    };
}

/// 记录 Alert 级别日志
#[macro_export]
macro_rules! alert {
    ($($arg:tt)*) => {
        $crate::logger_entry_fmt($crate::LogLevel::Alert, $crate::get_timestamp_ns(), ::core::format_args!($($arg)*))
    };
}

/// 记录 Critical 级别日志
#[macro_export]
macro_rules! critical {
    ($($arg:tt)*) => {
        $crate::logger_entry_fmt($crate::LogLevel::Critical, $crate::get_timestamp_ns(), ::core::format_args!($($arg)*))
    };
}

/// 记录 Error 级别日志
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::logger_entry_fmt($crate::LogLevel::Error, $crate::get_timestamp_ns(), ::core::format_args!($($arg)*))
    };
}

/// 记录 Warning 级别日志
#[macro_export]
macro_rules! warning {
    ($($arg:tt)*) => {
        $crate::logger_entry_fmt($crate::LogLevel::Warning, $crate::get_timestamp_ns(), ::core::format_args!($($arg)*))
    };
}

/// 记录 Notice 级别日志
#[macro_export]
macro_rules! notice {
    ($($arg:tt)*) => {
        $crate::logger_entry_fmt($crate::LogLevel::Notice, $crate::get_timestamp_ns(), ::core::format_args!($($arg)*))
    };
}

/// 记录 Info 级别日志
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::logger_entry_fmt($crate::LogLevel::Info, $crate::get_timestamp_ns(), ::core::format_args!($($arg)*))
    };
}

/// 记录 Debug 级别日志
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::logger_entry_fmt($crate::LogLevel::Debug, $crate::get_timestamp_ns(), ::core::format_args!($($arg)*))
    };
}

struct StackMessageBuffer<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> StackMessageBuffer<N> {
    const fn new() -> Self {
        Self {
            buf: [0u8; N],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}

impl<const N: usize> fmt::Write for StackMessageBuffer<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.len >= N {
            return Ok(());
        }

        let available = N - self.len;
        let copy_len = if s.len() <= available {
            s.len()
        } else {
            let mut end = 0usize;
            for (idx, ch) in s.char_indices() {
                let next = idx + ch.len_utf8();
                if next > available {
                    break;
                }
                end = next;
            }
            end
        };

        if copy_len != 0 {
            self.buf[self.len..self.len + copy_len].copy_from_slice(&s.as_bytes()[..copy_len]);
            self.len += copy_len;
        }

        Ok(())
    }
}

#[cfg(any(test, feature = "ktest-kernel"))]
mod tests;
