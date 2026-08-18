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
    DeviceNumberPolicy::char("full", 1, 7, "mem"),
    DeviceNumberPolicy::char("kmsg", 1, 11, "mem"),
    // 经典内存/IO 端口字符设备(与 Linux mem major 1 对齐)。
    DeviceNumberPolicy::char("mem", 1, 1, "mem"),
    DeviceNumberPolicy::char("kmem", 1, 2, "mem"),
    DeviceNumberPolicy::char("port", 1, 4, "mem"),
    // loop-control 是 Linux misc 设备(major 10),次号固定 237。
    DeviceNumberPolicy::char("loop-control", 10, 237, "misc"),
    DeviceNumberPolicy::char("console", 5, 1, "console"),
    // 控制终端与虚拟终端(与 Linux devtmpfs 布局一致)。
    DeviceNumberPolicy::char("tty", 5, 0, "console"),
    DeviceNumberPolicy::char("ptmx", 5, 2, "console"),
    DeviceNumberPolicy::char("tty0", 4, 0, "tty"),
    DeviceNumberPolicy::char("tty1", 4, 1, "tty"),
    DeviceNumberPolicy::char("tty2", 4, 2, "tty"),
    DeviceNumberPolicy::char("tty3", 4, 3, "tty"),
    DeviceNumberPolicy::char("tty4", 4, 4, "tty"),
    DeviceNumberPolicy::char("tty5", 4, 5, "tty"),
    DeviceNumberPolicy::char("tty6", 4, 6, "tty"),
    DeviceNumberPolicy::char("tty7", 4, 7, "tty"),
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
