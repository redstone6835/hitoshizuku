//! 标准用户 ABI 设备号策略。
//!
//! 本模块只声明用户态 ABI 需要的传统 `dev_t` 映射。它们不参与底层设备身份、
//! PnP 匹配或驱动资源所有权，只影响 `/dev`、`stat(2)`、`/proc/devices` 和
//! `/sys/dev/*` 这条用户可见投影链路。

use super::device_numbers::{
    DeviceNumberPolicy, DeviceNumberPolicyError, register_device_number_policy,
};

const WELL_KNOWN_DEVICE_POLICIES: &[DeviceNumberPolicy] = &[
    DeviceNumberPolicy::char("null", 1, 3, "mem"),
    DeviceNumberPolicy::char("zero", 1, 5, "mem"),
    DeviceNumberPolicy::char("random", 1, 8, "mem"),
    DeviceNumberPolicy::char("urandom", 1, 9, "mem"),
    DeviceNumberPolicy::char("console", 5, 1, "console"),
];

/// 安装当前内核支持的标准设备号策略。
///
/// 启动期会在 devtmpfs 绑定任何节点前调用。重复调用是安全的；如果两个兼容策略
/// 抢占同一节点名或同一 `dev_t`，注册表会返回明确错误，避免静默覆盖。
pub fn register_standard_device_policies() -> Result<(), DeviceNumberPolicyError> {
    for policy in WELL_KNOWN_DEVICE_POLICIES.iter().copied() {
        register_device_number_policy(policy)?;
    }
    Ok(())
}
