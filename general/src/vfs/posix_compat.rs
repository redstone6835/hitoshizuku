//! POSIX 兼容层设备策略。
//!
//! 本模块只声明用户态 ABI 需要的传统策略，例如 well-known `dev_t` 映射。
//! 这些策略不参与底层设备身份、PnP 匹配或驱动资源所有权，只影响 `/dev`、
//! `stat(2)`、`/proc/devices` 和 `/sys/dev/*` 这条兼容投影链路。

use super::device_numbers::{
    PosixDeviceNumberPolicy, PosixDevicePolicyError, register_device_number_policy,
};

// TODO(posix-compat): 这里是当前内核支持的传统 well-known `dev_t` 策略集合。
// 它已经从设备号分配器中移出，但仍是集中声明；后续应允许 tty、基础字符设备、
// 随机数设备、块设备别名等兼容模块分别注册自己的 policy，避免本文件继续增长
// 成新的兼容策略硬编码表。
const WELL_KNOWN_DEVICE_POLICIES: &[PosixDeviceNumberPolicy] = &[
    PosixDeviceNumberPolicy::char("null", 1, 3, "mem"),
    PosixDeviceNumberPolicy::char("zero", 1, 5, "mem"),
    PosixDeviceNumberPolicy::char("random", 1, 8, "mem"),
    PosixDeviceNumberPolicy::char("urandom", 1, 9, "mem"),
    PosixDeviceNumberPolicy::char("console", 5, 1, "console"),
];

/// 安装当前内核支持的 POSIX 设备号策略。
///
/// 启动期会在 devtmpfs 绑定任何节点前调用。重复调用是安全的；如果两个兼容策略
/// 抢占同一节点名或同一 `dev_t`，注册表会返回明确错误，避免静默覆盖。
pub fn register_posix_device_policies() -> Result<(), PosixDevicePolicyError> {
    for policy in WELL_KNOWN_DEVICE_POLICIES.iter().copied() {
        register_device_number_policy(policy)?;
    }
    Ok(())
}
