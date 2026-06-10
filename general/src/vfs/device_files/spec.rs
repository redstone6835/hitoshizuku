//! 设备文件投影规格。
//!
//! 这里集中描述 `/dev` 名字空间需要创建的节点。底层设备对象只通过 typed
//! `CharDevice`/`BlockDevice` 或 opaque payload 暴露能力；设备号、inode 元数据和
//! ioctl ABI 解释均留在 VFS/user_api 层，不能回流到底层设备模型。

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use vfs::error::{VfsError, VfsResult};

use crate::dev::block::BlockDevice;
use crate::dev::char::CharDevice;

/// 自定义 devtmpfs 节点的通用文件类别。
///
/// 该类型是设备文件投影层的中立描述，不暴露 `vfs::stat::FileType`。具体 inode
/// 类型转换只允许在 devtmpfs 适配层完成。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustomDevNodeKind {
    CharDevice,
    BlockDevice,
    RegularFile,
    Directory,
}

/// 自定义 devtmpfs 节点规格。
///
/// 新设备类型如果需要在 `/dev` 暴露特殊节点，只在这里声明节点名称、通用类别
/// 和 opaque payload。payload 的 ABI/VFS 解释由 devtmpfs 适配层负责，底层 dev
/// core 不依赖 `InodeOps`、`FileMode`、`Uid/Gid`、兼容设备号或 inode 元数据类型。
#[derive(Clone)]
pub struct CustomDevNodeSpec {
    name: Box<str>,
    kind: CustomDevNodeKind,
    payload: Arc<dyn Any + Send + Sync>,
}

impl CustomDevNodeSpec {
    pub fn try_new(
        name: &str,
        kind: CustomDevNodeKind,
        payload: Arc<dyn Any + Send + Sync>,
    ) -> VfsResult<Self> {
        Ok(Self {
            name: fallible_box_str(name)?,
            kind,
            payload,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> CustomDevNodeKind {
        self.kind
    }

    pub fn payload(&self) -> Arc<dyn Any + Send + Sync> {
        Arc::clone(&self.payload)
    }
}

/// devtmpfs 需要创建的兼容层设备节点。
///
/// 枚举变体直接携带 VFS 打开节点时需要的对象，因此 devtmpfs 不需要再把
/// `DeviceFunction` downcast 回 `CharFunction` 或 `BlockFunction`。
/// 这里描述的是用户态命名空间里的投影，不是底层硬件 identity；底层身份仍由
/// PnP id、function `class_id + dev_name` 和具体 typed device object 表达。
#[non_exhaustive]
#[derive(Clone)]
pub enum DevNodeSpec {
    Char {
        name: Box<str>,
        dev: CharDevice,
    },
    Block {
        name: Box<str>,
        dev: Arc<BlockDevice>,
    },
    Symlink {
        name: Box<str>,
        target: Box<str>,
    },
    Custom(CustomDevNodeSpec),
}

impl DevNodeSpec {
    pub fn name(&self) -> &str {
        match self {
            Self::Char { name, .. } | Self::Block { name, .. } | Self::Symlink { name, .. } => name,
            Self::Custom(spec) => spec.name(),
        }
    }

    pub fn custom(spec: CustomDevNodeSpec) -> Self {
        Self::Custom(spec)
    }
}

/// 一个 function 在 devtmpfs 中需要投影出的节点集合。
///
/// 旧接口里 `function 名称 == /dev 节点名 == 解绑键`，这会把设备身份和 VFS
/// 名字空间耦合在一起。节点集合把这三件事拆开：function 仍由 `class_id +
/// dev_name` 唯一标识，devtmpfs 只消费这里声明的路径投影。
#[derive(Clone)]
pub struct DevNodeSet {
    nodes: Vec<DevNodeSpec>,
}

impl DevNodeSet {
    pub fn try_single(node: DevNodeSpec) -> VfsResult<Self> {
        let mut nodes = Vec::new();
        nodes.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
        nodes.push(node);
        Ok(Self { nodes })
    }

    pub fn new(nodes: Vec<DevNodeSpec>) -> Option<Self> {
        if nodes.is_empty() {
            None
        } else {
            Some(Self { nodes })
        }
    }

    /// 构造一个已经完成名字唯一性校验的节点集合。
    ///
    /// 多个 projector 可以共同为同一个 function 生成投影，冲突必须在 VFS 投影层
    /// 被显式发现，而不是等到 devtmpfs 插入目录项时才留下半完成状态。
    pub fn try_new(nodes: Vec<DevNodeSpec>) -> VfsResult<Option<Self>> {
        validate_unique_node_names(&nodes)?;
        Ok(Self::new(nodes))
    }

    pub fn nodes(&self) -> &[DevNodeSpec] {
        &self.nodes
    }

    pub fn into_nodes(self) -> Vec<DevNodeSpec> {
        self.nodes
    }
}

pub fn fallible_box_str(value: &str) -> VfsResult<Box<str>> {
    let mut out = String::new();
    out.try_reserve(value.len())
        .map_err(|_| VfsError::NoSpace)?;
    out.push_str(value);
    Ok(out.into_boxed_str())
}

fn validate_unique_node_names(nodes: &[DevNodeSpec]) -> VfsResult<()> {
    for (idx, node) in nodes.iter().enumerate() {
        if nodes[..idx]
            .iter()
            .any(|existing| existing.name() == node.name())
        {
            return Err(VfsError::AlreadyExists);
        }
    }
    Ok(())
}
