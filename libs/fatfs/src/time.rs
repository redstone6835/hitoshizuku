//! FAT 时间戳编码/解码。
//!
//! FAT 使用独特的 DOS 时间戳:
//! - 日期字段:`YYYYYYY MMMM DDDDD`(年从 1980 起 7 位、月份 4 位、日 5 位)
//! - 时间字段:`HHHHH MMMMMM SSSSS`(秒精度 2 秒:5 位时、6 位分、5 位秒)
//! - 创建时间另有一个 10ms 精度字段(0..199,即百分之一秒)。
//!
//! 编码/解码遵循 Linux `vfat`(`fat_time_unix2fat` / `fat_time_fat2unix`)语义:
//! - 以 UTC 进行日历换算(即 Linux 的 `time64_to_tm(ts, 0, &tm)`,不叠加本地
//!   时区与 DST 偏移);
//! - 早于 1980 的时间截断到 1980-01-01 00:00:00;
//! - 晚于 2107 的时间截断到 2107-12-31 23:59:58(可表示的最大值);
//! - 秒按 2 秒粒度向下取整。

use vfs::stat::Timespec;

/// 默认的 DOS 时间戳(1980-01-01 00:00:00.00),即 `(time, date, tenths)`。
const EPOCH_1980: (u16, u16, u8) = (0, 0x0021, 0);

/// FAT 目录项承载的三组时间戳字段。
///
/// FAT 只有一个"创建时间"(create)一个"最后修改时间"(mtime)和一个"最后访问日期"
/// (adate,只存日期不含时分秒);没有独立的 Unix ctime 字段。写入新目录项时创建与
/// 修改取同一时刻,访问日期取当日。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FatTimestamp {
    /// 创建时间(create time)。
    pub time: u16,
    /// 创建日期(create date)。
    pub date: u16,
    /// 创建时间 10ms 分量(0..199)。
    pub tenths: u8,
    /// 最后修改时间。
    pub mtime: u16,
    /// 最后修改日期。
    pub mdate: u16,
    /// 最后访问日期(FAT 只存日期,时分秒视为 00:00:00)。
    pub adate: u16,
}

impl FatTimestamp {
    /// 用当前 Unix realtime 构造;未安装时钟时 [`Timespec::now`] 退回 Unix 纪元,
    /// 编码后自然截断为 1980 纪元,与既有行为一致。
    pub(crate) fn now() -> Self {
        Self::from_timespec(Timespec::now())
    }

    /// 由单个 [`Timespec`] 构造:创建、修改取同一时刻,访问日期取当日。
    pub(crate) fn from_timespec(ts: Timespec) -> Self {
        let (time, date, tenths) = timespec_to_fat(ts);
        Self {
            time,
            date,
            tenths,
            mtime: time,
            mdate: date,
            adate: date,
        }
    }
}

/// 把 Unix 天数(自 1970-01-01 起,可负)换算为公历 `(year, month, day)`。
///
/// 采用 Howard Hinnant 的 `civil_from_days` 算法,全程整数运算,适用于 `no_std`。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], 3 月起
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// 把公历 `(year, month, day)` 换算为自 1970-01-01 起的天数。
///
/// 与 [`civil_from_days`] 互逆,同样是 Hinnant 的 `days_from_civil` 算法。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = ((m + 9) % 12) as i64; // [0, 11], 3 月起
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// 将 [`Timespec`] 编码为 FAT 的 `(time, date, tenths)` 元组。
///
/// 边界处理与 Linux `vfat` 一致:早于 1980 截断到 1980-01-01,晚于 2107 截断到
/// 2107-12-31 23:59:58;秒按 2 秒粒度向下取整,10ms 分量取自纳秒。
pub(crate) fn timespec_to_fat(ts: Timespec) -> (u16, u16, u8) {
    let days = ts.secs.div_euclid(86_400);
    let secs_of_day = ts.secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if year < 1980 {
        return EPOCH_1980;
    }
    if year > 2107 {
        // 可表示最大值:127 年偏移 + 12 月 31 日;时间 23:59:58(2 秒粒度下最大偶数秒)。
        return (
            (23 << 11) | (59 << 5) | 29,
            (127 << 9) | (12 << 5) | 31,
            199,
        );
    }
    let hour = (secs_of_day / 3_600) as u16;
    let minute = ((secs_of_day % 3_600) / 60) as u16;
    let second = (secs_of_day % 60) as u16;
    let time = (hour << 11) | (minute << 5) | (second >> 1);
    let date = (((year - 1980) as u16) << 9) | ((month as u16) << 5) | (day as u16);
    // 10ms 分量 = 纳秒 / 10_000_000,取值 0..99(百分之一秒)。
    let tenths = (ts.nsecs / 10_000_000) as u8;
    (time, date, tenths)
}

/// 解码一个 FAT 时间戳为 [`Timespec`]。
///
/// `tenths` 字段名义上写作"十分之一秒",实际是 10ms 单位(0..199)。Linux 直接
/// 将其乘 10ms 放进 `tv_nsec`(对 100..199 会得到 ≥1s 的非法纳秒);这里把 ≥100
/// 的部分进位到秒,保证 `nsecs` 始终落在 `[0, 1_000_000_000)`。
pub(crate) fn fat_to_timespec(time: u16, date: u16, tenths: u8) -> Timespec {
    let year = ((date >> 9) & 0x7f) as i64 + 1980;
    let month = ((date >> 5) & 0x0f) as u32;
    let day = (date & 0x1f) as u32;
    let hour = ((time >> 11) & 0x1f) as i64;
    let minute = ((time >> 5) & 0x3f) as i64;
    let second = ((time & 0x1f) * 2) as i64;
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    let hundredths = tenths as i64;
    let secs = secs + hundredths / 100;
    let nsecs = ((hundredths % 100) * 10_000_000) as u32;
    Timespec { secs, nsecs }
}
