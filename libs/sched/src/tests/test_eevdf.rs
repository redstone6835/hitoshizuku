//! EEVDF 调度算法参数测试。
//!
//! 覆盖权重表查询（nice → weight）、边界钳制、SchedParams 默认值与委托。
//! 权重表数值来自 Linux kernel/sched/core.c::sched_prio_to_weight，
//! 相邻 nice 级比率约 1.25。

extern crate std;

use ktest::ktest;
use crate::eevdf::{weight_from_nice, SchedParams, NICE_0_WEIGHT, DEFAULT_BASE_SLICE_NS};

/// nice=-20 对应最高权重 88761。
#[ktest]
fn weight_from_nice_n20() {
    assert_eq!(weight_from_nice(-20), 88761);
}

/// nice=0 对应基准权重 1024。
#[ktest]
fn weight_from_nice_0() {
    assert_eq!(weight_from_nice(0), 1024);
    assert_eq!(weight_from_nice(0), NICE_0_WEIGHT);
}

/// nice=19 对应最低权重 15。
#[ktest]
fn weight_from_nice_19() {
    assert_eq!(weight_from_nice(19), 15);
}

/// nice 越界时自动钳制到 [-20, 19]，不会 panic。
#[ktest]
fn weight_from_nice_clamped() {
    assert_eq!(weight_from_nice(-30), 88761);
    assert_eq!(weight_from_nice(99), 15);
}

/// default_fair() 返回 nice=0、默认时间片、权重 1024。
#[ktest]
fn default_fair_params() {
    let p = SchedParams::default_fair();
    assert_eq!(p.nice, 0);
    assert_eq!(p.slice_ns, DEFAULT_BASE_SLICE_NS);
    assert_eq!(p.weight(), NICE_0_WEIGHT);
}

/// SchedParams.weight() 委托 weight_from_nice，行为一致。
#[ktest]
fn params_weight_delegates() {
    let p = SchedParams { nice: 5, slice_ns: 0 };
    assert_eq!(p.weight(), weight_from_nice(5));
}

/// slice_ns 为 0 时 slice() 返回默认基准时间片。
#[ktest]
fn params_slice_defaults_when_zero() {
    let p = SchedParams { nice: 0, slice_ns: 0 };
    assert_eq!(p.slice(), DEFAULT_BASE_SLICE_NS);
}

/// slice_ns 显式设置时 slice() 返回该值。
#[ktest]
fn params_slice_uses_explicit() {
    let p = SchedParams { nice: 0, slice_ns: 10_000_000 };
    assert_eq!(p.slice(), 10_000_000);
}
