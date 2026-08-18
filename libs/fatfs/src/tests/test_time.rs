//! FAT 时间戳编码/解码测试。

extern crate std;

use crate::time::{fat_to_timespec, timespec_to_fat};
use ktest::ktest;
use vfs::stat::Timespec;

/// Unix 纪元(1970-01-01)早于 FAT 可表示范围,应截断到 1980-01-01 00:00:00。
#[ktest]
fn timespec_to_fat_pre_1980_clamps_to_epoch() {
    assert_eq!(timespec_to_fat(Timespec::ZERO), (0, 0x0021, 0));
    assert_eq!(
        timespec_to_fat(Timespec { secs: -1, nsecs: 0 }),
        (0, 0x0021, 0)
    );
}

/// 晚于 2107 的时间截断到可表示最大值 2107-12-31 23:59:58。
#[ktest]
fn timespec_to_fat_post_2107_clamps_to_max() {
    let (time, date, tenths) = timespec_to_fat(Timespec {
        secs: 5_000_000_000,
        nsecs: 999_999_999,
    });
    assert_eq!(time, (23 << 11) | (59 << 5) | 29);
    assert_eq!(date, (127 << 9) | (12 << 5) | 31);
    assert_eq!(tenths, 199);
}

/// 已知 Unix 时间 2020-01-02 03:04:06.250 UTC 编码为预期 FAT 字段。
#[ktest]
fn timespec_to_fat_known_value() {
    let (time, date, tenths) = timespec_to_fat(Timespec {
        secs: 1_577_934_246,
        nsecs: 250_000_000,
    });
    assert_eq!(time, (3 << 11) | (4 << 5) | 3);
    assert_eq!(date, (40 << 9) | (1 << 5) | 2);
    assert_eq!(tenths, 25);
}

/// 同一组 FAT 字段解码回对应的 Unix 秒与 10ms 分量。
#[ktest]
fn fat_to_timespec_known_value() {
    let ts = fat_to_timespec((3 << 11) | (4 << 5) | 3, (40 << 9) | (1 << 5) | 2, 25);
    assert_eq!(ts.secs, 1_577_934_246);
    assert_eq!(ts.nsecs, 250_000_000);
}

/// FAT 纪元(1980-01-01 00:00:00)解码为对应 Unix 秒。
#[ktest]
fn fat_epoch_decodes_to_1980() {
    let ts = fat_to_timespec(0, 0x0021, 0);
    assert_eq!(ts.secs, 315_532_800);
    assert_eq!(ts.nsecs, 0);
}

/// 2 秒粒度:奇数秒编码后向下取整到偶数秒。
#[ktest]
fn fat_second_granularity_rounds_down() {
    let (time, date, tenths) = timespec_to_fat(Timespec {
        secs: 1_577_934_247,
        nsecs: 0,
    });
    let decoded = fat_to_timespec(time, date, tenths);
    assert_eq!(decoded.secs, 1_577_934_246);
}

/// 编码→解码往返:偶数秒与 10ms 分量保持一致。
#[ktest]
fn round_trip_preserves_even_second_and_tenths() {
    let ts = Timespec {
        secs: 1_700_000_000,
        nsecs: 450_000_000,
    };
    let (time, date, tenths) = timespec_to_fat(ts);
    let back = fat_to_timespec(time, date, tenths);
    assert_eq!(back.secs, ts.secs);
    assert_eq!(back.nsecs, ts.nsecs);
}
