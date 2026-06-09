//! 基础字符设备文件投影。
//!
//! `null/zero/random/urandom` 没有固件节点，也不需要 PnP 发现；它们是内核
//! 基础服务在 `/dev` 下的用户可见入口。因此路径、设备号和 devtmpfs 静态节点
//! 注册都放在 VFS 设备文件适配层，底层驱动只保留 typed I/O 能力。

use crate::dev::char::CharDevice;
use crate::dev::drivers::{RANDOM_DRIVER, URANDOM_DRIVER};
use crate::dev::function::DevNodeSpec;
use crate::vfs::devtmpfs::{DevTmpfsStaticNode, register_static_dev_nodes};
use vfs::error::VfsResult;

const BASE_DEVICE_FILE_OWNER: &str = "base-device-files";

const BASE_STATIC_NODES: [DevTmpfsStaticNode; 4] = [
    DevTmpfsStaticNode::new(BASE_DEVICE_FILE_OWNER, "null", null_dev_node),
    DevTmpfsStaticNode::new(BASE_DEVICE_FILE_OWNER, "zero", zero_dev_node),
    DevTmpfsStaticNode::new(BASE_DEVICE_FILE_OWNER, "random", random_dev_node),
    DevTmpfsStaticNode::new(BASE_DEVICE_FILE_OWNER, "urandom", urandom_dev_node),
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

fn random_dev_node() -> DevNodeSpec {
    DevNodeSpec::Char {
        name: "random".into(),
        dev: CharDevice::new("random", &RANDOM_DRIVER),
    }
}

fn urandom_dev_node() -> DevNodeSpec {
    DevNodeSpec::Char {
        name: "urandom".into(),
        dev: CharDevice::new("urandom", &URANDOM_DRIVER),
    }
}

/// 注册基础字符设备的 `/dev` 投影。
///
/// 批量注册入口提供事务语义：如果某个节点失败，会撤销本轮已经发布的节点，
/// 避免 `/dev` 中出现半套基础设备。
pub fn register_static_nodes() -> VfsResult<()> {
    register_static_dev_nodes(&BASE_STATIC_NODES)
}
