//! 通用 RTC 数据结构。
//!
//! 这里不描述任何具体硬件寄存器，只提供 RTC driver 之间共享的日历时间校验
//! 与 Unix time 转换逻辑。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtcDateTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl RtcDateTime {
    pub fn new(
        year: u32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<Self> {
        let value = Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        };
        value.is_valid().then_some(value)
    }

    pub fn unix_time_ns(self) -> Option<u64> {
        if !self.is_valid() {
            return None;
        }

        let mut days = 0u64;
        for year in 1970..self.year {
            days = days.checked_add(if is_leap_year(year) { 366 } else { 365 })?;
        }
        for month in 1..self.month {
            days = days.checked_add(days_in_month(self.year, month)? as u64)?;
        }
        days = days.checked_add((self.day - 1) as u64)?;

        let seconds = days
            .checked_mul(86_400)?
            .checked_add((self.hour as u64).checked_mul(3_600)?)?
            .checked_add((self.minute as u64).checked_mul(60)?)?
            .checked_add(self.second as u64)?;
        seconds.checked_mul(1_000_000_000)
    }

    fn is_valid(self) -> bool {
        if !(1970..=9999).contains(&self.year) {
            return false;
        }
        if self.hour > 23 || self.minute > 59 || self.second > 59 {
            return false;
        }
        let Some(month_days) = days_in_month(self.year, self.month) else {
            return false;
        };
        self.day != 0 && self.day <= month_days
    }
}

fn days_in_month(year: u32, month: u32) -> Option<u32> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 => Some(if is_leap_year(year) { 29 } else { 28 }),
        _ => None,
    }
}

fn is_leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}
