//! FAT 时间戳编码/解码。
//!
//! FAT 使用独特的 DOS 时间戳:
//! - 日期字段:`YYYYYYY MMMM DDDDD`(年从 1980 起,7 位月份 4 位日 5 位)
//! - 时间字段:`HHHHH MMMMMM SSSSS`(秒精度 2 秒:5 位时 6 位分 5 位秒)
//! - 10ms 精度字段用于创建时间的亚秒。
//!
//! 项目中 `Timespec::now()` 是占位实现,写入时统一落 1980-01-01。

use vfs::stat::Timespec;

/// 默认的 DOS 时间戳(1980-01-01 00:00:00.00),以 `(time, date, tenths)`。
pub(crate) const EPOCH_1980: (u16, u16, u8) = (0, 0x0021, 0);

/// 将 [`Timespec`] 编码为 FAT 的 `(time, date, tenths)` 元组。
///
/// 当前时钟源是占位(`Timespec::ZERO`),一律落 1980 纪元。目录项写入走
/// [`EPOCH_1980`]。本函数保留作为接入真实时钟后的入口。
#[inline]
#[allow(dead_code)]
pub(crate) fn timespec_to_fat(_ts: Timespec) -> (u16, u16, u8) {
    EPOCH_1980
}

/// 解码一个 FAT 时间戳。返回 [`Timespec::ZERO`] 表示 1980 纪元,
/// 因项目内缺少真实日历,解码信息暂不携带回用户空间。
#[inline]
#[allow(dead_code)]
pub(crate) fn fat_to_timespec(_time: u16, _date: u16, _tenths: u8) -> Timespec {
    Timespec::ZERO
}
