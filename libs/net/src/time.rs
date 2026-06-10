//! 网络层自有时间类型。
//!
//! 协议调度、DHCP 租期和上层内核 tick 只需要单调时间语义，不应该把
//! 具体协议引擎的时间结构暴露到 `libs/net` 公共 API。这里用轻量值类型
//! 表示时间点和时间段；当前 smoltcp 后端需要的转换仅在 crate 内可见。

use core::ops;

/// 单调时间点。
///
/// 单位为微秒，零点由调用方定义（通常是系统启动时间）。允许负值，便于
/// 与协议引擎历史行为兼容；公共调用通常通过 [`Self::from_millis`] 传入
/// 调度器提供的毫秒时间戳。
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NetInstant {
    micros: i64,
}

impl NetInstant {
    pub const ZERO: Self = Self { micros: 0 };

    /// 从微秒构造时间点。
    pub const fn from_micros(micros: i64) -> Self {
        Self { micros }
    }

    /// 从毫秒构造时间点。乘法饱和，避免异常输入造成回绕。
    pub fn from_millis(millis: i64) -> Self {
        Self {
            micros: millis.saturating_mul(1_000),
        }
    }

    /// 从秒构造时间点。乘法饱和，避免异常输入造成回绕。
    pub fn from_secs(secs: i64) -> Self {
        Self {
            micros: secs.saturating_mul(1_000_000),
        }
    }

    /// 返回自零点起的总微秒数。
    pub const fn total_micros(self) -> i64 {
        self.micros
    }

    /// 返回自零点起的总毫秒数。
    pub const fn total_millis(self) -> i64 {
        self.micros / 1_000
    }

    /// 饱和地加上一段时间。
    pub fn saturating_add_duration(self, duration: NetDuration) -> Self {
        Self {
            micros: self
                .micros
                .saturating_add(duration.as_i64_micros_saturating()),
        }
    }

    /// 饱和地减去一段时间。
    pub fn saturating_sub_duration(self, duration: NetDuration) -> Self {
        Self {
            micros: self
                .micros
                .saturating_sub(duration.as_i64_micros_saturating()),
        }
    }

    pub(crate) fn into_smoltcp(self) -> smoltcp::time::Instant {
        smoltcp::time::Instant::from_micros(self.micros)
    }

    #[cfg(test)]
    pub(crate) fn from_smoltcp(instant: smoltcp::time::Instant) -> Self {
        Self::from_micros(instant.total_micros())
    }
}

impl ops::Add<NetDuration> for NetInstant {
    type Output = Self;

    fn add(self, rhs: NetDuration) -> Self::Output {
        self.saturating_add_duration(rhs)
    }
}

impl ops::Sub<NetDuration> for NetInstant {
    type Output = Self;

    fn sub(self, rhs: NetDuration) -> Self::Output {
        self.saturating_sub_duration(rhs)
    }
}

impl ops::Sub<NetInstant> for NetInstant {
    type Output = NetDuration;

    fn sub(self, rhs: NetInstant) -> Self::Output {
        NetDuration::from_micros(self.micros.saturating_sub(rhs.micros).unsigned_abs())
    }
}

/// 单调时间段。
///
/// 使用无符号微秒保存，适合表达 TCP keepalive、DHCP lease 等持续时间。
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NetDuration {
    micros: u64,
}

impl NetDuration {
    pub const ZERO: Self = Self { micros: 0 };
    pub const MAX: Self = Self { micros: u64::MAX };

    /// 从微秒构造时间段。
    pub const fn from_micros(micros: u64) -> Self {
        Self { micros }
    }

    /// 从毫秒构造时间段。乘法饱和，避免异常输入造成回绕。
    pub const fn from_millis(millis: u64) -> Self {
        Self {
            micros: millis.saturating_mul(1_000),
        }
    }

    /// 从秒构造时间段。乘法饱和，避免异常输入造成回绕。
    pub const fn from_secs(secs: u64) -> Self {
        Self {
            micros: secs.saturating_mul(1_000_000),
        }
    }

    /// 返回总微秒数。
    pub const fn total_micros(self) -> u64 {
        self.micros
    }

    /// 返回总毫秒数。
    pub const fn total_millis(self) -> u64 {
        self.micros / 1_000
    }

    fn as_i64_micros_saturating(self) -> i64 {
        self.micros.min(i64::MAX as u64) as i64
    }

    pub(crate) fn into_smoltcp(self) -> smoltcp::time::Duration {
        smoltcp::time::Duration::from_micros(self.micros)
    }

    #[cfg(test)]
    pub(crate) fn from_smoltcp(duration: smoltcp::time::Duration) -> Self {
        Self::from_micros(duration.total_micros())
    }
}

impl ops::Add<NetDuration> for NetDuration {
    type Output = Self;

    fn add(self, rhs: NetDuration) -> Self::Output {
        Self {
            micros: self.micros.saturating_add(rhs.micros),
        }
    }
}

impl ops::Sub<NetDuration> for NetDuration {
    type Output = Self;

    fn sub(self, rhs: NetDuration) -> Self::Output {
        Self {
            micros: self.micros.saturating_sub(rhs.micros),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_instant_roundtrips_through_smoltcp() {
        let instant = NetInstant::from_millis(12_345);
        assert_eq!(NetInstant::from_smoltcp(instant.into_smoltcp()), instant);
        assert_eq!(instant.total_millis(), 12_345);

        let duration = NetDuration::from_secs(7);
        assert_eq!(NetDuration::from_smoltcp(duration.into_smoltcp()), duration);
    }

    #[test]
    fn duration_addition_is_saturating() {
        let instant = NetInstant::from_micros(i64::MAX - 10);
        assert_eq!(
            (instant + NetDuration::from_micros(50)).total_micros(),
            i64::MAX
        );
        assert_eq!(
            (NetDuration::MAX + NetDuration::from_micros(1)),
            NetDuration::MAX
        );
    }
}
