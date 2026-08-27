//! 日志环形缓冲区测试。
//!
//! 通过全局 LOGGER + logger_entry 写入，验证 read_all/export_text/clear 的公共接口。
//! 所有测试共享全局 LOGGER，使用进程内锁避免并发干扰。

extern crate std;

use crate::{LOGGER, LogLevel, logger_entry};
use alloc::string::ToString;
use alloc::vec;
use ktest::ktest;
use std::string::String;
use std::vec::Vec;

/// 进程内串行锁，防止多个 test_buffer 测试并发操作全局 LOGGER。
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 写入一条日志后 read_all 能读到该条目。
#[ktest]
fn write_and_read_one_entry() {
    let _guard = TEST_LOCK.lock().unwrap();
    LOGGER.clear();
    logger_entry(LogLevel::Info, 0, "hello world");

    let entries: Vec<_> = LOGGER.read_all().collect();
    assert!(!entries.is_empty());
    assert!(entries.iter().any(|e| e.message.contains("hello world")));
}

/// 写入多条日志后 read_all 返回数量不少于写入数。
#[ktest]
fn write_multiple_entries() {
    let _guard = TEST_LOCK.lock().unwrap();
    LOGGER.clear();
    logger_entry(LogLevel::Info, 0, "first");
    logger_entry(LogLevel::Warning, 0, "second");

    let entries: Vec<_> = LOGGER.read_all().collect();
    assert!(entries.len() >= 2);
}

/// export_text 输出的文本包含日志消息内容。
#[ktest]
fn export_text_includes_message() {
    let _guard = TEST_LOCK.lock().unwrap();
    LOGGER.clear();
    logger_entry(LogLevel::Error, 0, "test_error_msg");

    let text = LOGGER.export_text(false);
    let s = String::from_utf8_lossy(&text);
    assert!(s.contains("test_error_msg"));
}

/// clear 后 read_all 返回空，验证清空操作有效。
#[ktest]
fn clear_removes_all_entries() {
    let _guard = TEST_LOCK.lock().unwrap();
    LOGGER.clear();
    logger_entry(LogLevel::Info, 0, "will be cleared");
    LOGGER.clear();
    let entries: Vec<_> = LOGGER.read_all().collect();
    assert!(entries.is_empty());
}

/// replay_ready 逐条回调未读日志并推进 read_pos（消费语义）。
#[ktest]
fn replay_ready_consumes_entries() {
    let _guard = TEST_LOCK.lock().unwrap();
    LOGGER.clear();
    logger_entry(LogLevel::Info, 1, "first");
    logger_entry(LogLevel::Warning, 2, "second");

    let mut replayed = Vec::new();
    let count = LOGGER.replay_ready(|level, timestamp, message| {
        replayed.push((level, timestamp, message.to_string()));
    });
    assert_eq!(count, 2);
    assert_eq!(replayed.len(), 2);
    assert!(replayed.iter().any(|(_, _, m)| m == "first"));
    assert!(replayed.iter().any(|(_, _, m)| m == "second"));
    assert_eq!(LOGGER.unread_len(), 0);

    // 再写一条，只应回放新增的这条。
    logger_entry(LogLevel::Error, 3, "third");
    let mut again = Vec::new();
    let count2 = LOGGER.replay_ready(|_, _, message| again.push(message.to_string()));
    assert_eq!(count2, 1);
    assert_eq!(again, vec!["third".to_string()]);
    assert_eq!(LOGGER.unread_len(), 0);
}

/// 无未读日志时 replay_ready 返回 0 且不回调。
#[ktest]
fn replay_ready_empty_buffer() {
    let _guard = TEST_LOCK.lock().unwrap();
    LOGGER.clear();
    let count = LOGGER.replay_ready(|_, _, _| panic!("must not be called"));
    assert_eq!(count, 0);
}

/// 模块级 replay_ready_logs 包装等价于直接调用 replay_ready。
#[ktest]
fn replay_ready_logs_module_level() {
    let _guard = TEST_LOCK.lock().unwrap();
    LOGGER.clear();
    logger_entry(LogLevel::Info, 0, "module_level");
    let mut messages = Vec::new();
    let count = crate::replay_ready_logs(|_, _, message| messages.push(message.to_string()));
    assert_eq!(count, 1);
    assert_eq!(messages, vec!["module_level".to_string()]);
}
