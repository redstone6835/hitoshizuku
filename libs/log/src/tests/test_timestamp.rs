//! format_timestamp 纳秒转换测试。
//!
//! 验证纳秒值到 (秒, 纳秒) 元组的纯函数分解。

#[cfg(not(feature = "ktest-kernel"))]
extern crate std;
#[cfg(feature = "ktest-kernel")]
extern crate alloc;

use ktest::ktest;
use crate::format_timestamp;

/// 零纳秒映射为 (0, 0)。
#[ktest]
fn zero_nanos() {
    assert_eq!(format_timestamp(0), (0, 0));
}

/// 恰好 1 秒映射为 (1, 0)。
#[ktest]
fn one_sec() {
    assert_eq!(format_timestamp(1_000_000_000), (1, 0));
}

/// 非整秒的值正确分解秒和纳秒余数。
#[ktest]
fn sec_and_nanos() {
    assert_eq!(format_timestamp(1_500_000_000), (1, 500_000_000));
}
