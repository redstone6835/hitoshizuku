//! 基础静态字符设备。
//!
//! 这类设备没有固件节点和 PnP backing device，但仍需要出现在 `/dev` 兼容视图中。
//! 驱动只声明 `DevNodeSpec`，实际 inode 创建、路径校验和冲突处理仍由 devtmpfs 完成。

use crate::dev::char::CharDevice;
use crate::dev::function::DevNodeSpec;
use crate::dev::pnp::PnpError;
use crate::vfs::devtmpfs::{DevTmpfsStaticNode, register_static_dev_nodes};

const OWNER: &str = "base-char-driver";
const STATIC_NODES: [DevTmpfsStaticNode; 2] = [
    DevTmpfsStaticNode::new(OWNER, "null", null_dev_node),
    DevTmpfsStaticNode::new(OWNER, "zero", zero_dev_node),
];

fn null_dev_node() -> DevNodeSpec {
    DevNodeSpec::Char {
        name: "null".into(),
        dev: CharDevice::null(),
    }
}

fn zero_dev_node() -> DevNodeSpec {
    DevNodeSpec::Char {
        name: "zero".into(),
        dev: CharDevice::zero(),
    }
}

/// 注册无需 PnP 枚举的基础字符设备节点。
///
/// 这里保持事务语义：如果第二个节点注册失败，会撤销本轮已经提交的第一个节点，
/// 避免 devtmpfs 中留下半套基础设备投影。
pub(super) fn register_builtin_driver() -> Result<(), PnpError> {
    register_static_dev_nodes(&STATIC_NODES).map_err(|_| PnpError::DevtmpfsError)
}
