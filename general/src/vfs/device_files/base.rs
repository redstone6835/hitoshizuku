//! 基础字符设备文件投影。
//!
//! `null/zero/random/urandom` 没有固件节点，也不需要 PnP 发现；它们是内核
//! 基础服务在 `/dev` 下的用户可见入口。因此路径、设备号和 devtmpfs 静态节点
//! 注册都放在 VFS 设备文件适配层，底层驱动只保留 typed I/O 能力。

use crate::dev::char::{CharDevice, CharIoError};
use crate::dev::random::{RANDOM_PROXY_DRIVER, URANDOM_PROXY_DRIVER};
use alloc::string::{String, ToString};

use crate::vfs::device_files::spec::{DevNodeSpec, fallible_box_str};
use crate::vfs::devtmpfs::{DevTmpfsStaticNode, register_static_dev_nodes};
use vfs::error::VfsResult;

const BASE_DEVICE_FILE_OWNER: &str = "base-device-files";

const BASE_STATIC_NODES: [DevTmpfsStaticNode; 4] = [
    DevTmpfsStaticNode::new(BASE_DEVICE_FILE_OWNER, "null", null_dev_node),
    DevTmpfsStaticNode::new(BASE_DEVICE_FILE_OWNER, "zero", zero_dev_node),
    DevTmpfsStaticNode::new(BASE_DEVICE_FILE_OWNER, "random", random_dev_node),
    DevTmpfsStaticNode::new(BASE_DEVICE_FILE_OWNER, "urandom", urandom_dev_node),
];

fn null_dev_node() -> VfsResult<DevNodeSpec> {
    Ok(DevNodeSpec::Char {
        name: fallible_box_str("null")?,
        dev: CharDevice::null(),
    })
}

fn zero_dev_node() -> VfsResult<DevNodeSpec> {
    Ok(DevNodeSpec::Char {
        name: fallible_box_str("zero")?,
        dev: CharDevice::zero(),
    })
}

fn random_dev_node() -> VfsResult<DevNodeSpec> {
    Ok(DevNodeSpec::Char {
        name: fallible_box_str("random")?,
        dev: CharDevice::new("random", &RANDOM_PROXY_DRIVER),
    })
}

fn urandom_dev_node() -> VfsResult<DevNodeSpec> {
    Ok(DevNodeSpec::Char {
        name: fallible_box_str("urandom")?,
        dev: CharDevice::new("urandom", &URANDOM_PROXY_DRIVER),
    })
}

/// 注册基础字符设备的 `/dev` 投影。
///
/// 批量注册入口提供事务语义：如果某个节点失败，会撤销本轮已经发布的节点，
/// 避免 `/dev` 中出现半套基础设备。
pub fn register_static_nodes() -> VfsResult<()> {
    register_static_dev_nodes(&BASE_STATIC_NODES)
}

// ───────── 标准节点权限策略与 full/kmsg ─────────

const FULL_NODE_NAME: &str = "full";
const KMSG_NODE_NAME: &str = "kmsg";

/// /dev/full:读返回零,写恒 ENOSPC(Linux 语义)。
struct FullCharDriver;

impl crate::dev::char::CharDriver for FullCharDriver {
    fn write(&self, _buf: &[u8]) -> Result<usize, CharIoError> {
        Err(CharIoError::NoSpace)
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn poll_read(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// /dev/kmsg:内核日志环的字符视图。
struct KmsgCharDriver;

impl crate::dev::char::CharDriver for KmsgCharDriver {
    fn write(&self, _buf: &[u8]) -> Result<usize, CharIoError> {
        Ok(0)
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError> {
        let mut text = String::new();
        for record in log::LOGGER.read_all() {
            let (secs, nanos) = log::format_timestamp(record.timestamp);
            let _ = alloc::fmt::write(
                &mut text,
                format_args!("[{:6}.{:06}] {}\n", secs, nanos / 1000, record.message),
            );
        }
        if text.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(text.len());
        buf[..n].copy_from_slice(&text.as_bytes()[..n]);
        Ok(n)
    }

    fn poll_read(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn full_dev_node() -> VfsResult<DevNodeSpec> {
    Ok(DevNodeSpec::Char {
        name: fallible_box_str(FULL_NODE_NAME)?,
        dev: CharDevice::from_arc(
            fallible_box_str(FULL_NODE_NAME)?,
            alloc::sync::Arc::new(FullCharDriver),
        ),
    })
}

fn kmsg_dev_node() -> VfsResult<DevNodeSpec> {
    Ok(DevNodeSpec::Char {
        name: fallible_box_str(KMSG_NODE_NAME)?,
        dev: CharDevice::from_arc(
            fallible_box_str(KMSG_NODE_NAME)?,
            alloc::sync::Arc::new(KmsgCharDriver),
        ),
    })
}

/// 注册标准节点的权限策略(与 Linux devtmpfs 的用户可见权限对齐)。
///
/// 当前没有组模型:tty 类节点使用 0600 root:root,待 cred 组支持后
/// 切换为 0620 root:tty。
pub fn register_standard_node_policies() -> VfsResult<()> {
    let mut register = |name: &'static str, mode: u16| {
        crate::vfs::devtmpfs::register_node_policy(
            name,
            crate::vfs::devtmpfs::DevNodePolicy::new(mode),
        )
    };
    register("null", 0o666)?;
    register("zero", 0o666)?;
    register("random", 0o666)?;
    register("urandom", 0o666)?;
    register("full", 0o666)?;
    register("kmsg", 0o600)?;
    register("tty", 0o666)?;
    register("ptmx", 0o666)?;
    register("console", 0o600)?;
    register("tty0", 0o600)?;
    register("tty1", 0o600)?;
    register("tty2", 0o600)?;
    register("tty3", 0o600)?;
    register("tty4", 0o600)?;
    register("tty5", 0o600)?;
    register("tty6", 0o600)?;
    register("tty7", 0o600)?;
    Ok(())
}

/// 注册 full/kmsg 静态节点。
pub fn register_extra_static_nodes() -> VfsResult<()> {
    register_static_dev_nodes(&[
        DevTmpfsStaticNode::new(BASE_DEVICE_FILE_OWNER, FULL_NODE_NAME, full_dev_node),
        DevTmpfsStaticNode::new(BASE_DEVICE_FILE_OWNER, KMSG_NODE_NAME, kmsg_dev_node),
    ])
}
