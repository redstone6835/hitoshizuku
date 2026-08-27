//! 调度属性规范化与校验测试。
//!
//! 验证 SchedPolicy 的原始值解析与分类映射、SchedAttr 的 normalized() 自动填充
//! 与 validate() 合法性检查。normalized 会修正非法输入，validate 会拒绝原始非法输入，
//! 两者语义不同。

extern crate std;

use crate::sched_class::{SchedAttr, SchedClass, SchedPolicy};
use errno::Errno;
use ktest::ktest;

/// from_raw 对已知策略值返回 Some。
#[ktest]
fn policy_from_raw_valid() {
    assert_eq!(SchedPolicy::from_raw(0), Some(SchedPolicy::Fair));
    assert_eq!(SchedPolicy::from_raw(4), Some(SchedPolicy::Idle));
    assert_eq!(SchedPolicy::from_raw(5), Some(SchedPolicy::Batch));
}

/// from_raw 对未定义值返回 None。
#[ktest]
fn policy_from_raw_invalid() {
    assert_eq!(SchedPolicy::from_raw(255), None);
}

/// Fair 策略映射到 Fair 调度类。
#[ktest]
fn policy_class_fair() {
    assert_eq!(SchedPolicy::Fair.class(), SchedClass::Fair);
}

/// Batch 策略也映射到 Fair 调度类（nice 权重调度）。
#[ktest]
fn policy_class_batch() {
    assert_eq!(SchedPolicy::Batch.class(), SchedClass::Fair);
}

/// RtFifo 和 RtRoundRobin 均映射到 Realtime 调度类。
#[ktest]
fn policy_class_realtime() {
    assert_eq!(SchedPolicy::RtFifo.class(), SchedClass::Realtime);
    assert_eq!(SchedPolicy::RtRoundRobin.class(), SchedClass::Realtime);
}

/// Deadline 策略映射到 Deadline 调度类。
#[ktest]
fn policy_class_deadline() {
    assert_eq!(SchedPolicy::Deadline.class(), SchedClass::Deadline);
}

/// fair(10, 0) 经 normalized 后 nice 保持，slice 自动填充默认值。
#[ktest]
fn attr_fair_normalized() {
    let a = SchedAttr::fair(10, 0);
    let n = a.normalized();
    assert_eq!(n.nice, 10);
    assert!(n.slice_ns > 0);
}

/// rt_fifo(0) 经 normalized 后 priority 被 clamp 到 RT_PRIO_MIN=1。
#[ktest]
fn attr_rt_priority_clamped() {
    let n = SchedAttr::rt_fifo(0).normalized();
    assert_eq!(n.priority, 1);
}

/// deadline(0,0,0) 经 normalized 后三值自动填充默认值。
#[ktest]
fn attr_deadline_normalized() {
    let n = SchedAttr::deadline(0, 0, 0).normalized();
    assert!(n.runtime_ns > 0);
    assert!(n.deadline_ns > 0);
    assert!(n.period_ns > 0);
}

/// validate 拒绝 RT priority=0 的原始非法输入（与 normalized 不同）。
#[ktest]
fn attr_validate_rt_bad_prio() {
    assert_eq!(SchedAttr::rt_fifo(0).validate(), Err(Errno::EINVAL));
}

/// validate 接受 priority 在 [1, 99] 内的 RT 属性。
#[ktest]
fn attr_validate_rt_valid_prio() {
    assert!(SchedAttr::rt_fifo(1).validate().is_ok());
    assert!(SchedAttr::rt_fifo(99).validate().is_ok());
}

/// validate 拒绝 runtime > deadline 的 deadline 属性。
#[ktest]
fn attr_validate_deadline_runtime_gt_deadline() {
    assert_eq!(
        SchedAttr::deadline(20, 10, 30).validate(),
        Err(Errno::EINVAL)
    );
}

/// validate 拒绝 deadline > period 的 deadline 属性。
#[ktest]
fn attr_validate_deadline_deadline_gt_period() {
    assert_eq!(SchedAttr::deadline(1, 10, 5).validate(), Err(Errno::EINVAL));
}

/// Fair 属性无论 nice 值如何，validate 始终通过（normalized 会钳制）。
#[ktest]
fn attr_validate_fair_always_ok() {
    assert!(SchedAttr::fair(-30, 0).validate().is_ok());
    assert!(SchedAttr::fair(99, 0).validate().is_ok());
}
