//! 日志环形缓冲区测试。
//!
//! 通过全局 LOGGER + logger_entry 写入，验证 read_all/export_text/clear 的公共接口。

extern crate std;

use ktest::ktest;
use crate::{logger_entry, LogLevel, LOGGER};
use std::string::String;
use std::vec::Vec;

/// 写入一条日志后 read_all 能读到该条目。
#[ktest]
fn write_and_read_one_entry() {
    LOGGER.clear();
    logger_entry(LogLevel::Info, 0, "hello world");

    let entries: Vec<_> = LOGGER.read_all().collect();
    assert!(!entries.is_empty());
    assert!(entries.iter().any(|e| e.message.contains("hello world")));
}

/// 写入多条日志后 read_all 返回数量不少于写入数。
#[ktest]
fn write_multiple_entries() {
    LOGGER.clear();
    logger_entry(LogLevel::Info, 0, "first");
    logger_entry(LogLevel::Warning, 0, "second");

    let entries: Vec<_> = LOGGER.read_all().collect();
    assert!(entries.len() >= 2);
}

/// export_text 输出的文本包含日志消息内容。
#[ktest]
fn export_text_includes_message() {
    LOGGER.clear();
    logger_entry(LogLevel::Error, 0, "test_error_msg");

    let text = LOGGER.export_text(false);
    let s = String::from_utf8_lossy(&text);
    assert!(s.contains("test_error_msg"));
}

/// clear 后 read_all 返回空，验证清空操作有效。
#[ktest]
fn clear_removes_all_entries() {
    LOGGER.clear();
    logger_entry(LogLevel::Info, 0, "will be cleared");
    LOGGER.clear();
    let entries: Vec<_> = LOGGER.read_all().collect();
    assert!(entries.is_empty());
}
