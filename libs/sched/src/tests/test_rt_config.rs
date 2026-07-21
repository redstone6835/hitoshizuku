//! RT 调度运行时参数校验测试。

use errno::Errno;
use ktest::ktest;

use crate::DEFAULT_RR_SLICE_NS;
use crate::scheduler::{RtSchedulingConfig, normalize_rr_timeslice_ms};

#[ktest]
fn rt_period_rejects_nonpositive_overflow_and_runtime_conflict() {
    let config = RtSchedulingConfig::DEFAULT;
    assert_eq!(config.with_period_us(0), Err(Errno::EINVAL));
    assert_eq!(config.with_period_us(-1), Err(Errno::EINVAL));
    assert_eq!(
        config.with_period_us(i32::MAX as i64 + 1),
        Err(Errno::EINVAL)
    );
    assert_eq!(config.with_period_us(949_999), Err(Errno::EINVAL));
    assert!(config.with_period_us(950_000).is_ok());
}

#[ktest]
fn rt_runtime_accepts_unlimited_and_rejects_invalid_values() {
    let config = RtSchedulingConfig::DEFAULT;
    assert!(config.with_runtime_us(-1).is_ok());
    assert!(config.with_runtime_us(0).is_ok());
    assert!(config.with_runtime_us(1_000_000).is_ok());
    assert_eq!(config.with_runtime_us(-2), Err(Errno::EINVAL));
    assert_eq!(config.with_runtime_us(1_000_001), Err(Errno::EINVAL));
}

#[ktest]
fn rr_nonpositive_value_restores_default_timeslice() {
    let default_ms = (DEFAULT_RR_SLICE_NS / 1_000_000) as i32;
    assert_eq!(normalize_rr_timeslice_ms(25), Ok(25));
    assert_eq!(normalize_rr_timeslice_ms(-1), Ok(default_ms));
    assert_eq!(normalize_rr_timeslice_ms(0), Ok(default_ms));
    assert_eq!(
        normalize_rr_timeslice_ms(i32::MAX as i64 + 1),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        normalize_rr_timeslice_ms(i32::MIN as i64 - 1),
        Err(Errno::EINVAL)
    );
}
