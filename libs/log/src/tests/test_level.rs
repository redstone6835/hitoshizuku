//! 日志级别测试。
//!
//! 验证 LogLevel 的字符串表示与全局级别阈值读写。

extern crate std;

use crate::{LogLevel, get_log_level, set_log_level};
use ktest::ktest;

/// set_log_level 后 get_log_level 返回一致值。
#[ktest]
fn set_and_get_level() {
    set_log_level(LogLevel::Warning);
    assert_eq!(get_log_level(), LogLevel::Warning);
    set_log_level(LogLevel::Info);
}

/// as_str 返回各日志级别的标准缩写，与 Linux dmesg 输出格式一致。
#[ktest]
fn level_as_str() {
    assert_eq!(LogLevel::Emergency.as_str(), "EMERG");
    assert_eq!(LogLevel::Error.as_str(), "ERR");
    assert_eq!(LogLevel::Info.as_str(), "INFO");
    assert_eq!(LogLevel::Debug.as_str(), "DEBUG");
}
